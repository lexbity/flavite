//! Headless synthetic tests for the S1 IME event-delivery probe (slice A5; AC-24 instrument).
//!
//! These tests construct the custom widget, drive its `Widget::update` with
//! synthetic iced events (the exact types at the pinned iced 0.14 — names
//! verified against the vendored crate source), and assert the widget records
//! preedit text, that commit replaces it, and that every event variant is
//! captured. They prove *our* handling of the delivered events; OS-level
//! delivery is answered by the winit support matrix recorded in
//! `devdocs/spikes/S1-ime-delivery.md`.
//!
//! The widget is driven with `()` as its renderer — iced_core's debug null
//! renderer — so no GPU, window, or event loop is required. CI runs these on
//! Linux, macOS, and Windows via `cargo test --workspace` (G0/G1).

use iced::advanced::clipboard::Null as NullClipboard;
use iced::advanced::input_method;
use iced::advanced::layout::{Limits, Node};
use iced::advanced::text::Hit;
use iced::advanced::widget::Tree;
use iced::advanced::widget::tree;
use iced::advanced::{InputMethod, Layout, Shell, Widget};
use iced::event::{Event, Status};
use iced::keyboard::{self, Key, key};
use iced::mouse;
use iced::time::Instant;
use iced::touch;
use iced::window;
use iced::{Length, Pixels, Point, Rectangle, Size};
use std::ops::Range;
use std::path::PathBuf;
use xtask::s1_probe::{
    ImeProbe, ImeProbeState, LOG_CAPACITY, RecordedEvent, caret_from_hit, input_method_event_name,
};

/// The probe widget instantiated with iced_core's debug null renderer `()`.
type Probe = ImeProbe<()>;

/// What the shell observed after one fired event.
#[derive(Debug)]
struct Fired {
    status: Status,
    input_method: InputMethod<String>,
    redraw: window::RedrawRequest,
    messages: usize,
}

/// A driven probe: the widget, its state tree, and helpers to fire events.
struct Harness {
    widget: Probe,
    tree: Tree,
    node: Node,
    viewport: Rectangle,
}

impl Harness {
    fn new() -> Self {
        let widget = Probe::new().echo(false);
        // `Theme` is unconstrained by the probe's Widget impl; pin it to `()`.
        let tree = Tree {
            tag: Widget::<(), (), ()>::tag(&widget),
            state: Widget::<(), (), ()>::state(&widget),
            children: Widget::<(), (), ()>::children(&widget),
        };
        Self {
            widget,
            tree,
            node: Node::new(Size::new(720.0, 480.0)),
            viewport: Rectangle::with_size(Size::new(720.0, 480.0)),
        }
    }

    fn state(&self) -> &ImeProbeState {
        self.tree.state.downcast_ref::<ImeProbeState>()
    }

    /// Fires one event at the widget with the cursor at the given position.
    fn fire_at(&mut self, event: &Event, cursor: mouse::Cursor) -> Fired {
        let mut messages: Vec<()> = Vec::new();
        let mut shell = Shell::new(&mut messages);
        let mut clipboard = NullClipboard;
        Widget::<(), (), ()>::update(
            &mut self.widget,
            &mut self.tree,
            event,
            Layout::new(&self.node),
            cursor,
            &(),
            &mut clipboard,
            &mut shell,
            &self.viewport,
        );
        Fired {
            status: shell.event_status(),
            input_method: shell.input_method().clone(),
            redraw: shell.redraw_request(),
            messages: messages.len(),
        }
    }

    /// Fires one event with the cursor unavailable (no pointer interaction).
    fn fire(&mut self, event: &Event) -> Fired {
        self.fire_at(event, mouse::Cursor::Unavailable)
    }

    /// Fires a left click at the given position.
    fn click(&mut self, x: f32, y: f32) -> Fired {
        self.fire_at(
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            mouse::Cursor::Available(Point::new(x, y)),
        )
    }

    /// Clicks into the input area, focusing the widget.
    fn focus(&mut self) {
        let fired = self.click(10.0, 10.0);
        assert!(
            self.state().is_focused(),
            "click into the input area must focus the widget"
        );
        assert_eq!(fired.status, Status::Captured);
    }
}

// ---------------------------------------------------------------------------
// Event constructors (the exact iced 0.14 types)
// ---------------------------------------------------------------------------

fn key_pressed(key: Key, text: Option<&str>) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: key::Physical::Code(key::Code::KeyA),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::default(),
        text: text.map(Into::into),
        repeat: false,
    })
}

fn key_released(key: Key) -> Event {
    Event::Keyboard(keyboard::Event::KeyReleased {
        key: key.clone(),
        modified_key: key,
        physical_key: key::Physical::Code(key::Code::KeyA),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::default(),
    })
}

fn type_char(c: char) -> Event {
    let mut buf = [0u8; 4];
    key_pressed(
        Key::Character(c.encode_utf8(&mut buf).into()),
        Some(c.encode_utf8(&mut buf)),
    )
}

fn ime_opened() -> Event {
    Event::InputMethod(input_method::Event::Opened)
}

fn ime_preedit(content: &str, selection: Option<Range<usize>>) -> Event {
    Event::InputMethod(input_method::Event::Preedit(content.to_owned(), selection))
}

fn ime_commit(text: &str) -> Event {
    Event::InputMethod(input_method::Event::Commit(text.to_owned()))
}

fn ime_closed() -> Event {
    Event::InputMethod(input_method::Event::Closed)
}

fn redraw_requested() -> Event {
    Event::Window(window::Event::RedrawRequested(Instant::now()))
}

// ---------------------------------------------------------------------------
// IME event recording (the S1 question, our side of it)
// ---------------------------------------------------------------------------

#[test]
fn records_every_ime_event_variant() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&ime_opened());
    h.fire(&ime_preedit("かな", Some(0..2)));
    h.fire(&ime_commit("かな"));
    h.fire(&ime_closed());

    let state = h.state();
    assert_eq!(state.count("input_method.opened"), 1);
    assert_eq!(state.count("input_method.preedit"), 1);
    assert_eq!(state.count("input_method.commit"), 1);
    assert_eq!(state.count("input_method.closed"), 1);

    let variants: Vec<&'static str> = state.log().iter().map(|e| e.variant).collect();
    assert!(variants.contains(&"input_method.opened"));
    assert!(variants.contains(&"input_method.preedit"));
    assert!(variants.contains(&"input_method.commit"));
    assert!(variants.contains(&"input_method.closed"));
}

#[test]
fn preedit_is_recorded_but_not_committed() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&ime_opened());
    h.fire(&ime_preedit("かな", Some(0..2)));

    let state = h.state();
    // The preedit is live state, never buffer content before commit.
    assert_eq!(state.preedit().map(|p| p.content.as_str()), Some("かな"));
    assert_eq!(
        state.preedit().and_then(|p| p.selection.clone()),
        Some(0..2)
    );
    assert_eq!(state.text(), "");
    assert!(state.is_ime_open());

    // ...and it is recorded with its text in the log detail.
    let last = state.log().back().expect("preedit must be recorded");
    assert_eq!(last.variant, "input_method.preedit");
    assert!(last.detail.contains("かな"), "detail: {}", last.detail);
}

#[test]
fn commit_inserts_text_and_clears_preedit() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&type_char('a'));
    h.fire(&type_char('b'));
    h.fire(&ime_opened());
    h.fire(&ime_preedit("か", None));

    let fired = h.fire(&ime_commit("かな"));

    // The commit replaces the preedit: text lands in the buffer, preedit clears.
    assert_eq!(h.state().text(), "abかな");
    assert_eq!(h.state().preedit(), None);
    assert_eq!(fired.status, Status::Captured);
    assert_eq!(fired.redraw, window::RedrawRequest::NextFrame);
    assert_eq!(fired.messages, 0, "the probe publishes no messages");
}

#[test]
fn commit_inserts_at_caret() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&type_char('a'));
    h.fire(&type_char('b'));
    h.fire(&key_pressed(Key::Named(key::Named::ArrowLeft), None));

    h.fire(&ime_commit("X"));

    assert_eq!(h.state().text(), "aXb");
    assert_eq!(h.state().caret(), 2);
}

#[test]
fn empty_preedit_event_clears_the_preedit_content() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&ime_opened());
    h.fire(&ime_preedit("かな", None));
    h.fire(&ime_preedit("", None));

    let preedit = h.state().preedit().expect("preedit still tracked");
    assert_eq!(preedit.content, "");
    assert_eq!(preedit.selection, None);
}

#[test]
fn closed_clears_preedit_and_ime_state() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&ime_opened());
    h.fire(&ime_preedit("かな", None));

    let fired = h.fire(&ime_closed());

    assert!(!h.state().is_ime_open());
    assert_eq!(h.state().preedit(), None);
    assert_eq!(fired.status, Status::Captured);
}

#[test]
fn ime_events_are_recorded_but_not_applied_when_unfocused() {
    let mut h = Harness::new();
    // No click — the widget is unfocused.
    h.fire(&ime_opened());
    h.fire(&ime_preedit("かな", None));
    h.fire(&ime_commit("かな"));

    let state = h.state();
    // Recording is unconditional — the probe's job.
    assert_eq!(state.count("input_method.opened"), 1);
    assert_eq!(state.count("input_method.preedit"), 1);
    assert_eq!(state.count("input_method.commit"), 1);
    // ...but composition only applies while focused (mirrors iced's text_input):
    // the Preedit/Commit were not applied, so the preedit is still the empty one
    // that `Opened` installed — never the composition text.
    assert_eq!(state.text(), "");
    assert_eq!(
        state.preedit().map(|p| p.content.as_str()),
        Some(""),
        "the composition text must not be applied while unfocused"
    );
    // Opened/Closed are not focus-gated (mirrors iced's text_input).
    assert!(state.is_ime_open());
}

#[test]
fn ime_event_names_cover_exactly_the_pinned_variants() {
    // `input_method_event_name` is deliberately exhaustive (no wildcard arm):
    // winit 0.30 / iced 0.14 deliver exactly these four IME events, and no
    // candidate-list event exists at these versions. If a future iced/winit
    // adds a variant, this test's sibling function stops compiling.
    let all = [
        input_method::Event::Opened,
        input_method::Event::Preedit(String::new(), None),
        input_method::Event::Commit(String::new()),
        input_method::Event::Closed,
    ];
    let names: Vec<&'static str> = all.iter().map(input_method_event_name).collect();
    assert_eq!(
        names,
        vec![
            "input_method.opened",
            "input_method.preedit",
            "input_method.commit",
            "input_method.closed",
        ]
    );
}

// ---------------------------------------------------------------------------
// IME strategy (cursor-area reporting + preedit handoff)
// ---------------------------------------------------------------------------

#[test]
fn redraw_requests_input_method_when_focused() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&ime_opened());
    h.fire(&ime_preedit("かな", Some(0..2)));

    let fired = h.fire(&redraw_requested());

    let InputMethod::Enabled {
        cursor,
        purpose,
        preedit,
    } = fired.input_method
    else {
        panic!(
            "expected InputMethod::Enabled, got {:?}",
            fired.input_method
        );
    };
    assert_eq!(purpose, input_method::Purpose::Normal);
    // Null renderer: grapheme_position is None, so the caret x is the origin.
    assert_eq!(
        cursor,
        Rectangle::new(Point::new(24.0, 24.0), Size::new(1.5, 26.0))
    );
    let preedit = preedit.expect("preedit handed to the runtime");
    assert_eq!(preedit.content, "かな");
    assert_eq!(preedit.selection, Some(0..2));
    assert_eq!(preedit.text_size, Some(Pixels(20.0)));
}

#[test]
fn input_method_is_disabled_when_unfocused() {
    let mut h = Harness::new();
    let fired = h.fire(&redraw_requested());
    assert_eq!(fired.input_method, InputMethod::Disabled);
}

// ---------------------------------------------------------------------------
// Keyboard and mouse handling
// ---------------------------------------------------------------------------

#[test]
fn keyboard_text_inserts_and_captures() {
    let mut h = Harness::new();
    h.focus();

    let fired = h.fire(&type_char('h'));

    assert_eq!(h.state().text(), "h");
    assert_eq!(h.state().caret(), 1);
    assert_eq!(fired.status, Status::Captured);
    assert_eq!(fired.redraw, window::RedrawRequest::NextFrame);
}

#[test]
fn keyboard_editing_keys_move_and_delete() {
    let mut h = Harness::new();
    h.focus();
    h.fire(&type_char('a'));
    h.fire(&type_char('b'));
    assert_eq!(h.state().text(), "ab");
    assert_eq!(h.state().caret(), 2);

    h.fire(&key_pressed(Key::Named(key::Named::ArrowLeft), None));
    assert_eq!(h.state().caret(), 1);

    h.fire(&key_pressed(Key::Named(key::Named::Backspace), None));
    assert_eq!(h.state().text(), "b");
    assert_eq!(h.state().caret(), 0);

    h.fire(&key_pressed(Key::Named(key::Named::Delete), None));
    assert_eq!(h.state().text(), "");
    assert_eq!(h.state().caret(), 0);

    h.fire(&key_pressed(Key::Named(key::Named::ArrowRight), None));
    assert_eq!(h.state().caret(), 0, "caret stays at 0 in empty text");

    h.fire(&type_char('z'));
    assert_eq!(h.state().text(), "z");
    h.fire(&key_pressed(Key::Named(key::Named::Home), None));
    assert_eq!(h.state().caret(), 0);
    h.fire(&key_pressed(Key::Named(key::Named::End), None));
    assert_eq!(h.state().caret(), 1);
}

#[test]
fn keyboard_is_ignored_when_unfocused() {
    let mut h = Harness::new();

    let fired = h.fire(&type_char('x'));

    assert_eq!(h.state().text(), "");
    assert_eq!(fired.status, Status::Ignored);
    assert_eq!(fired.redraw, window::RedrawRequest::Wait);
}

#[test]
fn enter_is_captured_but_inserts_nothing() {
    let mut h = Harness::new();
    h.focus();

    // Single-line probe: Enter is recorded (and captured) but inserts nothing,
    // even if the OS also produced text for the key press.
    let fired = h.fire(&key_pressed(Key::Named(key::Named::Enter), Some("\r")));

    assert_eq!(h.state().text(), "");
    assert_eq!(fired.status, Status::Captured);
}

#[test]
fn click_focuses_and_unfocuses() {
    let mut h = Harness::new();
    assert!(!h.state().is_focused());

    h.focus();

    // The null renderer's hit_test returns None, so the caret stays at 0;
    // hit-based caret placement is exercised with a real renderer (A6/A7).
    assert_eq!(h.state().caret(), 0);

    h.click(10.0, 400.0);
    assert!(!h.state().is_focused());
}

#[test]
fn caret_from_hit_maps_the_hit_to_a_caret() {
    assert_eq!(caret_from_hit(Some(Hit::CharOffset(2))), Some(2));
    assert_eq!(caret_from_hit(Some(Hit::CharOffset(0))), Some(0));
    assert_eq!(caret_from_hit(None), None);
}

// ---------------------------------------------------------------------------
// Recording: every event variant, ordering, ring buffer, format
// ---------------------------------------------------------------------------

#[test]
fn records_every_event_variant() {
    let mut h = Harness::new();

    let events: Vec<(Event, &'static str)> = vec![
        (
            key_pressed(Key::Character("a".into()), Some("a")),
            "keyboard.key_pressed",
        ),
        (
            key_released(Key::Character("a".into())),
            "keyboard.key_released",
        ),
        (
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::default(),
            )),
            "keyboard.modifiers_changed",
        ),
        (
            Event::Mouse(mouse::Event::CursorEntered),
            "mouse.cursor_entered",
        ),
        (Event::Mouse(mouse::Event::CursorLeft), "mouse.cursor_left"),
        (
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(1.0, 2.0),
            }),
            "mouse.cursor_moved",
        ),
        (
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            "mouse.button_pressed",
        ),
        (
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            "mouse.button_released",
        ),
        (
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            "mouse.wheel_scrolled",
        ),
        (
            Event::Window(window::Event::Opened {
                position: None,
                size: Size::new(720.0, 480.0),
            }),
            "window.opened",
        ),
        (Event::Window(window::Event::Closed), "window.closed"),
        (
            Event::Window(window::Event::Moved(Point::new(0.0, 0.0))),
            "window.moved",
        ),
        (
            Event::Window(window::Event::Resized(Size::new(720.0, 480.0))),
            "window.resized",
        ),
        (
            Event::Window(window::Event::Rescaled(1.0)),
            "window.rescaled",
        ),
        (redraw_requested(), "window.redraw_requested"),
        (
            Event::Window(window::Event::CloseRequested),
            "window.close_requested",
        ),
        (Event::Window(window::Event::Focused), "window.focused"),
        (Event::Window(window::Event::Unfocused), "window.unfocused"),
        (
            Event::Window(window::Event::FileHovered(PathBuf::from("note.md"))),
            "window.file_hovered",
        ),
        (
            Event::Window(window::Event::FileDropped(PathBuf::from("note.md"))),
            "window.file_dropped",
        ),
        (
            Event::Window(window::Event::FilesHoveredLeft),
            "window.files_hovered_left",
        ),
        (
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(0),
                position: Point::new(1.0, 1.0),
            }),
            "touch.finger_pressed",
        ),
        (
            Event::Touch(touch::Event::FingerMoved {
                id: touch::Finger(0),
                position: Point::new(2.0, 2.0),
            }),
            "touch.finger_moved",
        ),
        (
            Event::Touch(touch::Event::FingerLifted {
                id: touch::Finger(0),
                position: Point::new(2.0, 2.0),
            }),
            "touch.finger_lifted",
        ),
        (
            Event::Touch(touch::Event::FingerLost {
                id: touch::Finger(0),
                position: Point::new(2.0, 2.0),
            }),
            "touch.finger_lost",
        ),
        (ime_opened(), "input_method.opened"),
        (ime_preedit("か", None), "input_method.preedit"),
        (ime_commit("か"), "input_method.commit"),
        (ime_closed(), "input_method.closed"),
    ];

    for (event, _) in &events {
        h.fire(event);
    }

    let state = h.state();
    assert_eq!(state.total(), events.len() as u64);
    assert_eq!(state.log().len(), events.len());
    for (i, (_, variant)) in events.iter().enumerate() {
        assert_eq!(state.count(variant), 1, "variant {variant} recorded once");
        assert_eq!(
            state.log()[i].variant,
            *variant,
            "event {i} recorded in order"
        );
        assert_eq!(state.log()[i].seq, i as u64);
    }
    // Timestamps are non-decreasing.
    let mut prev = 0;
    for event in state.log() {
        assert!(event.at_ms >= prev, "timestamps must be non-decreasing");
        prev = event.at_ms;
    }
}

#[test]
fn log_ring_buffer_drops_oldest_past_capacity() {
    let mut h = Harness::new();
    let total = LOG_CAPACITY + 10;
    for _ in 0..total {
        h.fire(&Event::Window(window::Event::Focused));
    }

    let state = h.state();
    assert_eq!(state.total(), total as u64);
    assert_eq!(state.log().len(), LOG_CAPACITY);
    assert_eq!(state.log().front().map(|e| e.seq), Some(10));
    assert_eq!(state.log().back().map(|e| e.seq), Some(total as u64 - 1));
}

#[test]
fn recorded_event_render_pins_the_stdout_format() {
    let event = RecordedEvent {
        seq: 3,
        at_ms: 42,
        variant: "input_method.commit",
        detail: "commit=\"かな\"".to_owned(),
    };
    // The manual checklist greps stdout for these lines — the format is pinned.
    assert_eq!(
        event.render(),
        "[s1-probe      42 ms] input_method.commit        commit=\"かな\""
    );
}

// ---------------------------------------------------------------------------
// Widget contract: tag/state/layout
// ---------------------------------------------------------------------------

#[test]
fn state_tree_roundtrips_through_the_widget_contract() {
    let widget = Probe::new().echo(false);
    let tree = Tree {
        tag: Widget::<(), (), ()>::tag(&widget),
        state: Widget::<(), (), ()>::state(&widget),
        children: Widget::<(), (), ()>::children(&widget),
    };
    assert_eq!(tree.tag, tree::Tag::of::<ImeProbeState>());
    // The state downcasts — the widget's state type is ImeProbeState.
    let _ = tree.state.downcast_ref::<ImeProbeState>();
}

#[test]
fn layout_fills_the_available_space() {
    let mut widget = Probe::new().echo(false);
    let mut tree = Tree {
        tag: Widget::<(), (), ()>::tag(&widget),
        state: Widget::<(), (), ()>::state(&widget),
        children: Widget::<(), (), ()>::children(&widget),
    };
    assert_eq!(
        Widget::<(), (), ()>::size(&widget),
        Size::new(Length::Fill, Length::Fill)
    );
    let limits = Limits::new(Size::ZERO, Size::new(720.0, 480.0));
    let node = Widget::<(), (), ()>::layout(&mut widget, &mut tree, &(), &limits);
    assert_eq!(node.size(), Size::new(720.0, 480.0));
}
