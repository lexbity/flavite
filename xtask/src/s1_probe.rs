//! S1 IME event-delivery probe — spike S1, slice A5 (§9 Phase 0; D18; ADR-0001; AC-24).
//!
//! The S1 question this answers: **can IME preedit / commit / candidate events
//! reach a *custom* iced widget** on Linux (X11 + Wayland), macOS, and Windows at
//! the pinned versions (iced 0.14, winit 0.30)? ADR-0001 closed Q1 (the custom
//! editor is the decision); this probe verifies the custom path's one remaining
//! external dependency — IME event delivery.
//!
//! The probe is a minimal custom widget (via [`iced::advanced`]) that records
//! **every** runtime event it receives — with special recording for IME events
//! (preedit text, commit text) — into a ring buffer in its widget state, echoes
//! each event to stdout with a timestamp, and reacts like a tiny editor: it
//! keeps committed text, a caret, and the current preedit, and it requests the
//! IME strategy (cursor area + purpose + preedit) from the shell on redraw,
//! exactly like iced's own `text_input`.
//!
//! The widget is renderer-generic ([`ImeProbe<R>`]), which lets the headless
//! synthetic tests in `xtask/tests/s1_ime_probe.rs` drive it with `()` as the
//! renderer — iced_core's debug null renderer: no GPU, no window, no event
//! loop. The binary (`xtask/src/bin/s1_ime_probe.rs`) hosts the same widget as
//! `ImeProbe<iced::Renderer>` in a real iced application for manual per-OS
//! checklist runs (`devdocs/checklists/ime-acceptance.md`).
//!
//! Findings (per-OS event matrix, verdicts) are recorded in
//! `devdocs/spikes/S1-ime-delivery.md`.

use iced::advanced::clipboard::Clipboard;
use iced::advanced::layout::{self, Layout};
use iced::advanced::mouse;
use iced::advanced::renderer;
use iced::advanced::text::{self, LineHeight, Paragraph, Shaping, Text, Wrapping};
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{InputMethod, Shell, Widget, input_method};
use iced::event::Event;
use iced::keyboard::{self, Key, key};
use iced::time::Instant;
use iced::touch;
use iced::window;
use iced::{Border, Color, Font, Length, Pixels, Point, Rectangle, Size, alignment};
use std::collections::{BTreeMap, VecDeque};
use std::marker::PhantomData;
use std::ops::Range;

/// Maximum number of recorded events kept in the ring buffer.
pub const LOG_CAPACITY: usize = 512;

/// Outer margin of the input area, in logical pixels.
const MARGIN: f32 = 8.0;
/// Width of the drawn caret, in logical pixels.
const CARET_WIDTH: f32 = 1.5;
/// Gap between the input area and the event log, in logical pixels.
const LOG_GAP: f32 = 12.0;
/// Horizontal margin of the event log, in logical pixels.
const LOG_MARGIN: f32 = 12.0;
/// Line height of the event log, in logical pixels.
const LOG_LINE_HEIGHT: f32 = 18.0;
/// Font size of the event log, in logical pixels.
const LOG_FONT_SIZE: Pixels = Pixels(13.0);

const WINDOW_BG: Color = Color {
    r: 0.07,
    g: 0.08,
    b: 0.10,
    a: 1.0,
};
const INPUT_BG: Color = Color {
    r: 0.11,
    g: 0.12,
    b: 0.15,
    a: 1.0,
};
const INPUT_BORDER: Color = Color {
    r: 0.25,
    g: 0.27,
    b: 0.32,
    a: 1.0,
};

// ---------------------------------------------------------------------------
// Recorded events
// ---------------------------------------------------------------------------

/// A single recorded runtime event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEvent {
    /// Monotonic sequence number (0-based, per probe state).
    pub seq: u64,
    /// Milliseconds since the probe state was created.
    pub at_ms: u64,
    /// Stable variant name, e.g. `"input_method.preedit"`.
    pub variant: &'static str,
    /// Human-readable detail (preedit text, committed text, key name, ...).
    pub detail: String,
}

impl RecordedEvent {
    /// The exact line written to stdout when echoing is enabled.
    ///
    /// The manual checklist (`devdocs/checklists/ime-acceptance.md`) greps for
    /// these lines; the format is pinned by a test.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "[s1-probe {:>7} ms] {:<26} {}",
            self.at_ms, self.variant, self.detail
        )
    }
}

/// The live preedit (composition) state, as delivered by the IME.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreeditRecord {
    /// The current composition text.
    pub content: String,
    /// Byte-wise cursor range within `content`, as delivered by the IME.
    pub selection: Option<Range<usize>>,
}

// ---------------------------------------------------------------------------
// Widget state
// ---------------------------------------------------------------------------

/// Widget state, persisted in the widget [`Tree`] across view rebuilds.
///
/// All fields are private; tests and the app read them through the accessors.
#[derive(Debug)]
pub struct ImeProbeState {
    /// When the state was created — recorded timestamps are relative to this.
    started: Instant,
    /// Ring buffer of recorded events (oldest dropped past [`LOG_CAPACITY`]).
    log: VecDeque<RecordedEvent>,
    /// Committed text (what an editor buffer would hold).
    text: String,
    /// Byte caret position within `text` (always on a char boundary).
    caret: usize,
    /// Current preedit, if composing.
    preedit: Option<PreeditRecord>,
    /// Whether the IME reported itself open.
    ime_open: bool,
    /// Whether the widget is focused (clicked into).
    focused: bool,
    /// Per-variant event counts.
    counts: BTreeMap<&'static str, u64>,
    /// Total events seen (also the next sequence number).
    total: u64,
}

impl ImeProbeState {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            log: VecDeque::new(),
            text: String::new(),
            caret: 0,
            preedit: None,
            ime_open: false,
            focused: false,
            counts: BTreeMap::new(),
            total: 0,
        }
    }

    /// The committed text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The byte caret position within [`ImeProbeState::text`].
    #[must_use]
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The current preedit, if composing.
    #[must_use]
    pub fn preedit(&self) -> Option<&PreeditRecord> {
        self.preedit.as_ref()
    }

    /// Whether the IME reported itself open.
    #[must_use]
    pub fn is_ime_open(&self) -> bool {
        self.ime_open
    }

    /// Whether the widget is focused.
    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// The recorded events (oldest first).
    #[must_use]
    pub fn log(&self) -> &VecDeque<RecordedEvent> {
        &self.log
    }

    /// How many events with the given variant name were recorded.
    #[must_use]
    pub fn count(&self, variant: &str) -> u64 {
        self.counts.get(variant).copied().unwrap_or(0)
    }

    /// Total events recorded.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    fn record(&mut self, variant: &'static str, detail: String, echo: bool) {
        let at_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let event = RecordedEvent {
            seq: self.total,
            at_ms,
            variant,
            detail,
        };
        self.total += 1;
        *self.counts.entry(variant).or_insert(0) += 1;
        if echo {
            println!("{}", event.render());
        }
        self.log.push_back(event);
        if self.log.len() > LOG_CAPACITY {
            self.log.pop_front();
        }
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn set_ime_open(&mut self, open: bool) {
        self.ime_open = open;
        self.preedit = open.then(PreeditRecord::default);
    }

    fn set_preedit(&mut self, content: &str, selection: &Option<Range<usize>>) {
        self.preedit = Some(PreeditRecord {
            content: content.to_owned(),
            selection: selection.clone(),
        });
    }

    fn clear_preedit(&mut self) {
        self.preedit = None;
    }

    fn insert_at_caret(&mut self, text: &str) {
        self.text.insert_str(self.caret, text);
        self.caret += text.len();
    }

    fn delete_backward(&mut self) {
        if let Some(c) = self.text[..self.caret].chars().next_back() {
            let start = self.caret - c.len_utf8();
            self.text.replace_range(start..self.caret, "");
            self.caret = start;
        }
    }

    fn delete_forward(&mut self) {
        if let Some(c) = self.text[self.caret..].chars().next() {
            self.text
                .replace_range(self.caret..self.caret + c.len_utf8(), "");
        }
    }

    fn move_caret_left(&mut self) {
        if let Some(c) = self.text[..self.caret].chars().next_back() {
            self.caret -= c.len_utf8();
        }
    }

    fn move_caret_right(&mut self) {
        if let Some(c) = self.text[self.caret..].chars().next() {
            self.caret += c.len_utf8();
        }
    }

    fn move_caret_to_start(&mut self) {
        self.caret = 0;
    }

    fn move_caret_to_end(&mut self) {
        self.caret = self.text.len();
    }

    fn set_caret_from_hit(&mut self, hit: Option<text::Hit>) {
        if let Some(index) = caret_from_hit(hit) {
            self.caret = index.min(self.text.len());
        }
    }
}

/// The caret position for a paragraph hit-test result, if any.
///
/// A free function so the mapping is unit-testable without a real renderer
/// (the null renderer's `hit_test` always returns `None`).
#[must_use]
pub fn caret_from_hit(hit: Option<text::Hit>) -> Option<usize> {
    hit.map(text::Hit::cursor)
}

// ---------------------------------------------------------------------------
// The probe widget
// ---------------------------------------------------------------------------

/// The S1 probe widget: a minimal custom editor surface that records every
/// event it receives.
///
/// Renderer-generic so tests can use `()` (iced_core's debug null renderer);
/// the binary uses `ImeProbe<iced::Renderer>`. The widget itself is pure
/// configuration — all live state lives in [`ImeProbeState`] in the widget
/// tree, so a fresh `ImeProbe::new()` is constructed on every `view` call.
pub struct ImeProbe<R> {
    /// Echo recorded events to stdout (the manual checklist reads this).
    echo: bool,
    /// Font size of the input line, in logical pixels.
    font_size: Pixels,
    /// Inner padding of the input area, in logical pixels.
    padding: f32,
    /// How many log lines to draw.
    log_lines: usize,
    /// Renderer marker — see the struct docs.
    renderer: PhantomData<R>,
}

impl<R> ImeProbe<R> {
    /// Creates a new probe widget with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            echo: true,
            font_size: Pixels(20.0),
            padding: 16.0,
            log_lines: 16,
            renderer: PhantomData,
        }
    }

    /// Sets whether recorded events are echoed to stdout.
    #[must_use]
    pub fn echo(mut self, echo: bool) -> Self {
        self.echo = echo;
        self
    }

    /// Sets how many log lines are drawn.
    #[must_use]
    pub fn log_lines(mut self, log_lines: usize) -> Self {
        self.log_lines = log_lines;
        self
    }

    /// The bounds of the input area within the widget bounds.
    fn input_bounds(&self, bounds: Rectangle) -> Rectangle {
        let line = LineHeight::default().to_absolute(self.font_size).0;
        Rectangle {
            x: bounds.x + MARGIN,
            y: bounds.y + MARGIN,
            width: (bounds.width - MARGIN * 2.0).max(0.0),
            height: self.padding * 2.0 + line,
        }
    }

    /// The origin of the committed text within the input area.
    fn text_origin(&self, input_bounds: Rectangle) -> Point {
        Point::new(input_bounds.x + self.padding, input_bounds.y + self.padding)
    }

    /// Builds a single-line paragraph for the given content.
    fn paragraph_for(&self, content: &str, size: Pixels, bounds: Size) -> R::Paragraph
    where
        R: text::Renderer<Font = Font>,
    {
        R::Paragraph::with_text(Text {
            content,
            bounds,
            size,
            line_height: LineHeight::default(),
            font: Font::DEFAULT,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: Shaping::default(),
            wrapping: Wrapping::None,
        })
    }

    /// The IME strategy requested from the shell on redraw.
    ///
    /// Mirrors iced's own `text_input`: the cursor rectangle is reported to the
    /// OS (`set_ime_cursor_area` — candidate-window positioning) and the
    /// current preedit is handed to the runtime, which draws it as an
    /// over-the-spot overlay. When unfocused, the IME is disabled.
    fn input_method_strategy(
        &self,
        state: &ImeProbeState,
        layout: Layout<'_>,
    ) -> InputMethod<String>
    where
        R: text::Renderer<Font = Font>,
    {
        if !state.is_focused() {
            return InputMethod::Disabled;
        }
        let input_bounds = self.input_bounds(layout.bounds());
        let origin = self.text_origin(input_bounds);
        let text_bounds = Size::new(
            (input_bounds.width - self.padding * 2.0).max(0.0),
            input_bounds.height,
        );
        let paragraph = self.paragraph_for(state.text(), self.font_size, text_bounds);
        let caret_x = paragraph
            .grapheme_position(0, state.caret())
            .map_or(0.0, |position| position.x);
        let line_height = LineHeight::default().to_absolute(self.font_size).0;
        InputMethod::Enabled {
            cursor: Rectangle::new(
                Point::new(origin.x + caret_x, origin.y),
                Size::new(CARET_WIDTH, line_height),
            ),
            purpose: input_method::Purpose::Normal,
            preedit: state.preedit().map(|preedit| input_method::Preedit {
                content: preedit.content.clone(),
                selection: preedit.selection.clone(),
                text_size: Some(self.font_size),
            }),
        }
    }

    /// Handles a key press: editing keys move/delete, anything else with
    /// produced text inserts at the caret. Returns whether the event was
    /// handled (and should be captured).
    fn handle_key<Message>(
        &self,
        state: &mut ImeProbeState,
        key: &Key,
        text: Option<&str>,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        match key {
            Key::Named(key::Named::Backspace) => state.delete_backward(),
            Key::Named(key::Named::Delete) => state.delete_forward(),
            Key::Named(key::Named::ArrowLeft) => state.move_caret_left(),
            Key::Named(key::Named::ArrowRight) => state.move_caret_right(),
            Key::Named(key::Named::Home) => state.move_caret_to_start(),
            Key::Named(key::Named::End) => state.move_caret_to_end(),
            // Single-line probe: Enter is recorded but inserts nothing.
            Key::Named(key::Named::Enter) => {}
            _ => {
                let Some(text) = text else {
                    return false;
                };
                if text.is_empty() || text.chars().any(char::is_control) {
                    return false;
                }
                state.insert_at_caret(text);
            }
        }
        shell.capture_event();
        true
    }

    /// Handles a left click: focuses the input area and places the caret by
    /// hit-testing the committed text; clicking elsewhere unfocuses.
    fn handle_click<Message>(
        &self,
        state: &mut ImeProbeState,
        layout: Layout<'_>,
        position: Point,
        shell: &mut Shell<'_, Message>,
    ) where
        R: text::Renderer<Font = Font>,
    {
        let input_bounds = self.input_bounds(layout.bounds());
        if position.y >= input_bounds.y && position.y <= input_bounds.y + input_bounds.height {
            state.set_focused(true);
            let origin = self.text_origin(input_bounds);
            let text_bounds = Size::new(
                (input_bounds.width - self.padding * 2.0).max(0.0),
                input_bounds.height,
            );
            let paragraph = self.paragraph_for(state.text(), self.font_size, text_bounds);
            let local = Point::new(position.x - origin.x, position.y - origin.y);
            state.set_caret_from_hit(paragraph.hit_test(local));
            shell.request_redraw();
        } else {
            state.set_focused(false);
        }
        shell.capture_event();
    }
}

impl<R> Default for ImeProbe<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Message, Theme, R> Widget<Message, Theme, R> for ImeProbe<R>
where
    R: text::Renderer<Font = Font>,
{
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ImeProbeState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(ImeProbeState::new())
    }

    fn layout(&mut self, _tree: &mut Tree, _renderer: &R, limits: &layout::Limits) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut R,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<ImeProbeState>();
        let bounds = layout.bounds();

        renderer.fill_quad(
            renderer::Quad {
                bounds,
                ..renderer::Quad::default()
            },
            WINDOW_BG,
        );

        let input_bounds = self.input_bounds(bounds);
        renderer.fill_quad(
            renderer::Quad {
                bounds: input_bounds,
                border: Border {
                    color: INPUT_BORDER,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..renderer::Quad::default()
            },
            INPUT_BG,
        );

        let origin = self.text_origin(input_bounds);
        let text_bounds = Size::new(
            (input_bounds.width - self.padding * 2.0).max(0.0),
            input_bounds.height,
        );
        let paragraph = self.paragraph_for(state.text(), self.font_size, text_bounds);
        renderer.fill_paragraph(&paragraph, origin, style.text_color, *viewport);

        if state.is_focused() {
            let caret_x = paragraph
                .grapheme_position(0, state.caret())
                .map_or(0.0, |position| position.x);
            let line_height = LineHeight::default().to_absolute(self.font_size).0;
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(origin.x + caret_x, origin.y),
                        Size::new(CARET_WIDTH, line_height),
                    ),
                    ..renderer::Quad::default()
                },
                style.text_color,
            );
        }

        // Event log: the most recent `log_lines` entries, oldest first.
        let log_color = Color {
            a: 0.7,
            ..style.text_color
        };
        let log_bounds = Size::new((bounds.width - LOG_MARGIN * 2.0).max(0.0), LOG_LINE_HEIGHT);
        let mut y = input_bounds.y + input_bounds.height + LOG_GAP;
        for event in state.log().iter().rev().take(self.log_lines).rev() {
            let paragraph = self.paragraph_for(&event.render(), LOG_FONT_SIZE, log_bounds);
            renderer.fill_paragraph(
                &paragraph,
                Point::new(bounds.x + LOG_MARGIN, y),
                log_color,
                *viewport,
            );
            y += LOG_LINE_HEIGHT;
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &R,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<ImeProbeState>();

        // The probe's raison d'être: record every event, with IME detail.
        let (variant, detail) = describe(event);
        state.record(variant, detail, self.echo);

        match event {
            Event::InputMethod(ime) => match ime {
                input_method::Event::Opened => {
                    state.set_ime_open(true);
                    shell.request_redraw();
                    shell.capture_event();
                }
                input_method::Event::Closed => {
                    state.set_ime_open(false);
                    shell.request_redraw();
                    shell.capture_event();
                }
                input_method::Event::Preedit(content, selection) => {
                    if state.is_focused() {
                        state.set_preedit(content, selection);
                        shell.request_redraw();
                    }
                    shell.capture_event();
                }
                input_method::Event::Commit(text) => {
                    if state.is_focused() {
                        state.insert_at_caret(text);
                        state.clear_preedit();
                        shell.request_redraw();
                    }
                    shell.capture_event();
                }
            },
            Event::Keyboard(keyboard::Event::KeyPressed { key, text, .. }) => {
                if state.is_focused() && self.handle_key(state, key, text.as_deref(), shell) {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(position) = cursor.position_over(layout.bounds()) {
                    self.handle_click(state, layout, position, shell);
                }
            }
            Event::Window(window::Event::RedrawRequested(_)) => {
                shell.request_input_method(&self.input_method_strategy(state, layout));
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Event description (exhaustive)
// ---------------------------------------------------------------------------

/// Names every [`input_method::Event`] variant at the pinned iced version.
///
/// Deliberately exhaustive — there is no wildcard arm. winit 0.30 / iced 0.14
/// deliver exactly four IME events (`Opened`, `Preedit`, `Commit`, `Closed`);
/// **no candidate-list event exists at these versions** (winit's `Ime` enum
/// has no `Candidates` variant — candidate windows are OS-managed). If a
/// future iced/winit adds a variant, this function stops compiling and forces
/// a conscious revisit of the S1 finding in `devdocs/spikes/S1-ime-delivery.md`.
#[must_use]
pub fn input_method_event_name(event: &input_method::Event) -> &'static str {
    match event {
        input_method::Event::Opened => "input_method.opened",
        input_method::Event::Preedit(..) => "input_method.preedit",
        input_method::Event::Commit(..) => "input_method.commit",
        input_method::Event::Closed => "input_method.closed",
    }
}

/// Names and details an arbitrary runtime [`Event`].
///
/// Exhaustive over the iced 0.14 `Event` enum — every variant the widget can
/// receive gets a stable name and a human-readable detail line.
fn describe(event: &Event) -> (&'static str, String) {
    match event {
        Event::Keyboard(keyboard::Event::KeyPressed { key, text, .. }) => {
            ("keyboard.key_pressed", format!("key={key:?} text={text:?}"))
        }
        Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) => {
            ("keyboard.key_released", format!("key={key:?}"))
        }
        Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => (
            "keyboard.modifiers_changed",
            format!("modifiers={modifiers:?}"),
        ),
        Event::Mouse(mouse::Event::CursorEntered) => ("mouse.cursor_entered", String::new()),
        Event::Mouse(mouse::Event::CursorLeft) => ("mouse.cursor_left", String::new()),
        Event::Mouse(mouse::Event::CursorMoved { position }) => {
            ("mouse.cursor_moved", format!("position={position:?}"))
        }
        Event::Mouse(mouse::Event::ButtonPressed(button)) => {
            ("mouse.button_pressed", format!("button={button:?}"))
        }
        Event::Mouse(mouse::Event::ButtonReleased(button)) => {
            ("mouse.button_released", format!("button={button:?}"))
        }
        Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
            ("mouse.wheel_scrolled", format!("delta={delta:?}"))
        }
        Event::Window(window::Event::Opened { position, size }) => (
            "window.opened",
            format!("position={position:?} size={size:?}"),
        ),
        Event::Window(window::Event::Closed) => ("window.closed", String::new()),
        Event::Window(window::Event::Moved(position)) => {
            ("window.moved", format!("position={position:?}"))
        }
        Event::Window(window::Event::Resized(size)) => ("window.resized", format!("size={size:?}")),
        Event::Window(window::Event::Rescaled(scale_factor)) => {
            ("window.rescaled", format!("scale_factor={scale_factor}"))
        }
        Event::Window(window::Event::RedrawRequested(at)) => {
            ("window.redraw_requested", format!("at={at:?}"))
        }
        Event::Window(window::Event::CloseRequested) => ("window.close_requested", String::new()),
        Event::Window(window::Event::Focused) => ("window.focused", String::new()),
        Event::Window(window::Event::Unfocused) => ("window.unfocused", String::new()),
        Event::Window(window::Event::FileHovered(path)) => {
            ("window.file_hovered", format!("path={path:?}"))
        }
        Event::Window(window::Event::FileDropped(path)) => {
            ("window.file_dropped", format!("path={path:?}"))
        }
        Event::Window(window::Event::FilesHoveredLeft) => {
            ("window.files_hovered_left", String::new())
        }
        Event::Touch(touch::Event::FingerPressed { id, position }) => (
            "touch.finger_pressed",
            format!("id={id:?} position={position:?}"),
        ),
        Event::Touch(touch::Event::FingerMoved { id, position }) => (
            "touch.finger_moved",
            format!("id={id:?} position={position:?}"),
        ),
        Event::Touch(touch::Event::FingerLifted { id, position }) => (
            "touch.finger_lifted",
            format!("id={id:?} position={position:?}"),
        ),
        Event::Touch(touch::Event::FingerLost { id, position }) => (
            "touch.finger_lost",
            format!("id={id:?} position={position:?}"),
        ),
        Event::InputMethod(ime) => (input_method_event_name(ime), ime_detail(ime)),
    }
}

/// The detail line for an IME event — the part the manual checklist reads.
fn ime_detail(event: &input_method::Event) -> String {
    match event {
        input_method::Event::Opened => "ime opened".to_owned(),
        input_method::Event::Preedit(content, selection) => {
            format!("preedit={content:?} selection={selection:?}")
        }
        input_method::Event::Commit(text) => format!("commit={text:?}"),
        input_method::Event::Closed => "ime closed".to_owned(),
    }
}
