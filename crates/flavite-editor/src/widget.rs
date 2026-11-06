//! Custom iced editor widget over a revisioned, immutable API view.

use crate::keymap::KeyAction;
use crate::links::{
    LinkResolution, LinkSpan, NoteCandidate, match_candidates, resolve_link, scan_note,
};
use crate::surface::{EditorAction, EditorSurface, EditorView, LinkNavigation};
use crate::viewport::{
    Viewport, clamp_char_boundary, next_grapheme_boundary, previous_grapheme_boundary,
};
use cosmic_text::{Attrs, Buffer as ShapingBuffer, FontSystem, Metrics, Shaping};
use flavite_api::BufferView;
use flavite_syntax::SpanKind;
use iced::advanced::input_method::InputMethod;
use iced::advanced::{
    Clipboard, Layout, Renderer as AdvancedRenderer, Shell, Widget, input_method,
    layout::{Limits, Node},
    mouse, renderer, text,
    text::Renderer as TextRenderer,
    widget::Tree,
};
use iced::keyboard::{self, Key, Modifiers, key::Named};
use iced::{Background, Border, Color, Length, Point, Rectangle, Size, Theme};
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FONT_SIZE: f32 = 16.0;
const LINE_HEIGHT: f32 = 22.0;
const PAD_X: f32 = 12.0;
const PAD_Y: f32 = 10.0;
/// Scrollbar thumb width in logical pixels.
const SCROLLBAR_WIDTH: f32 = 8.0;
/// Hit-test gutter at the pane's right edge (thumb + click slop).
const SCROLLBAR_GUTTER: f32 = 16.0;

/// Scrollbar geometry for a pane of `height`: track top, track height,
/// thumb top, thumb height, and the maximum `first_line` (the viewport
/// stays full — the last page bounds the scroll).
fn scrollbar_geometry(
    height: f32,
    total_lines: usize,
    visible_lines: usize,
    first_line: usize,
) -> (f32, f32, f32, f32, usize) {
    let track_y = PAD_Y;
    let track_height = (height - PAD_Y * 2.0).max(1.0);
    let shown = visible_lines.min(total_lines).max(1);
    let max_first = total_lines.saturating_sub(shown);
    let thumb_height =
        (track_height * shown as f32 / total_lines.max(1) as f32).clamp(24.0, track_height);
    let travel = (track_height - thumb_height).max(1.0);
    let thumb_y = track_y + travel * (first_line.min(max_first) as f32 / max_first.max(1) as f32);
    (track_y, track_height, thumb_y, thumb_height, max_first)
}

/// The editor's type settings: font size in logical pixels and the derived
/// line height. The host sets these from its interface preferences (W11);
/// the default reproduces the historical fixed metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeMetrics {
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Line height in logical pixels; caret, scroll, and glyph math all
    /// derive from this single value.
    pub line_height: f32,
}

impl Default for TypeMetrics {
    fn default() -> Self {
        Self {
            font_size: FONT_SIZE,
            line_height: LINE_HEIGHT,
        }
    }
}

impl TypeMetrics {
    /// Metrics for a font size with the standard 1.4 line ratio, rounded
    /// to the nearest half pixel so positions stay stable.
    #[must_use]
    pub fn for_font_size(font_size: f32) -> Self {
        Self {
            font_size,
            line_height: (font_size * 1.4 * 2.0).round() / 2.0,
        }
    }
}

/// Host-provided semantic colors for editor chrome (W11): selection,
/// link families, text, preedit, and autocomplete. The host derives these
/// from its active scheme; the defaults reproduce the historical palette
/// so headless callers keep the previous rendering.
#[derive(Debug, Clone, Copy)]
pub struct EditorColors {
    /// Selection highlight fill.
    pub selection: Color,
    /// Resolved link underline.
    pub link: Color,
    /// Unresolved link underline.
    pub link_unresolved: Color,
    /// Ambiguous link underline.
    pub link_multiple: Color,
    /// Editor text fill.
    pub text: Color,
    /// Composition preedit underline.
    pub preedit_underline: Color,
    /// Composition preedit text.
    pub preedit_text: Color,
    /// Autocomplete popup background.
    pub autocomplete_background: Color,
    /// Autocomplete selected row.
    pub autocomplete_selected: Color,
    /// Autocomplete unselected row.
    pub autocomplete_text: Color,
    /// Autocomplete create-row accent.
    pub autocomplete_accent: Color,
}

impl Default for EditorColors {
    fn default() -> Self {
        Self {
            selection: Color::from_rgba(0.24, 0.42, 0.67, 0.55),
            link: Color::from_rgb(0.35, 0.67, 0.98),
            link_unresolved: Color::from_rgb(0.95, 0.48, 0.43),
            link_multiple: Color::from_rgb(0.96, 0.73, 0.32),
            text: Color::from_rgb(0.88, 0.90, 0.93),
            preedit_underline: Color::from_rgb(0.98, 0.78, 0.35),
            preedit_text: Color::from_rgb(1.0, 0.86, 0.55),
            autocomplete_background: Color::from_rgb(0.15, 0.17, 0.20),
            autocomplete_selected: Color::from_rgb(0.55, 0.78, 1.0),
            autocomplete_text: Color::from_rgb(0.82, 0.84, 0.87),
            autocomplete_accent: Color::from_rgb(0.45, 0.72, 1.0),
        }
    }
}

/// Interaction snapshots are published immediately on press/release/scroll;
/// during a drag they are coalesced to at most one per frame so a pointer
/// sweep cannot flood the host acknowledgement loop.
const INTERACTION_PUBLISH_INTERVAL: Duration = Duration::from_millis(16);

thread_local! {
    static FONT_SYSTEM: RefCell<FontSystem> = RefCell::new(FontSystem::new());
}

/// Custom editor surface. Mutations are emitted as revision-guarded actions;
/// this widget never mutates a buffer or accesses the filesystem itself.
pub struct EditorWidget<'a, Message> {
    view: EditorView,
    candidates: Arc<[NoteCandidate]>,
    link_resolutions: Arc<HashMap<String, LinkResolution>>,
    stable_links: bool,
    on_action: Box<dyn Fn(EditorAction) -> Message + 'a>,
    on_interaction: Option<Box<dyn Fn(EditorInteractionSnapshot) -> Message + 'a>>,
    initial_interaction: Option<EditorInteractionSnapshot>,
    view_key: Option<(u128, u64)>,
    input_enabled: bool,
    keyboard_enabled: bool,
    on_input_intent: Option<Box<dyn Fn(crate::input::CapturedInput) -> Message + 'a>>,
    width: Length,
    height: Length,
    /// Host-configured type settings (W11 editor font size).
    type_metrics: TypeMetrics,
    /// Optional readable-line-width cap in logical pixels (`None` = the
    /// full pane width minus padding). Text wraps inside this width and
    /// stays left-aligned; it is a wrap constraint, not a reflow anchor.
    max_line_width: Option<f32>,
    /// Host-provided semantic chrome colors (W11).
    colors: EditorColors,
    _message: PhantomData<Message>,
}

impl<'a, Message> EditorWidget<'a, Message> {
    /// Create an editor from the API's immutable revisioned view.
    pub fn new(view: &BufferView, on_action: impl Fn(EditorAction) -> Message + 'a) -> Self {
        Self::from_view(EditorView::from(view), on_action)
    }

    /// Create an editor from an already shared, API-owned text revision.
    pub fn from_view(view: EditorView, on_action: impl Fn(EditorAction) -> Message + 'a) -> Self {
        Self {
            view,
            candidates: Arc::from([]),
            link_resolutions: Arc::new(HashMap::new()),
            stable_links: false,
            on_action: Box::new(on_action),
            on_interaction: None,
            initial_interaction: None,
            view_key: None,
            input_enabled: true,
            keyboard_enabled: true,
            on_input_intent: None,
            width: Length::Fill,
            height: Length::Fill,
            type_metrics: TypeMetrics::default(),
            max_line_width: None,
            colors: EditorColors::default(),
            _message: PhantomData,
        }
    }

    /// Set the semantic chrome colors from the host's active scheme (W11).
    #[must_use]
    pub fn with_colors(mut self, colors: EditorColors) -> Self {
        self.colors = colors;
        self
    }

    /// Set the editor type settings from the host's preferences (W11).
    #[must_use]
    pub fn with_type_metrics(mut self, metrics: TypeMetrics) -> Self {
        self.type_metrics = metrics;
        self
    }

    /// Cap the wrap width (readable line width, W11). `None` restores the
    /// full pane width.
    #[must_use]
    pub fn with_max_line_width(mut self, max_line_width: Option<f32>) -> Self {
        self.max_line_width = max_line_width;
        self
    }

    /// The wrap width for a pane of `bounds.width`: the readable-line
    /// preference caps the text column; the pane width bounds it below.
    #[must_use]
    pub(crate) fn text_wrap_width(&self, bounds_width: f32) -> f32 {
        let available = (bounds_width - PAD_X * 2.0).max(1.0);
        match self.max_line_width {
            Some(max) => available.min(max.max(1.0)),
            None => available,
        }
    }

    /// Supply an indexed title/alias snapshot for in-memory autocomplete.
    #[must_use]
    pub fn with_candidates(mut self, candidates: Arc<[NoteCandidate]>) -> Self {
        self.candidates = candidates;
        self
    }

    /// Supply resolver verdicts keyed by the folded raw link target. Verdicts
    /// come from the index snapshot API; the editor does not rank candidates.
    #[must_use]
    pub fn with_link_resolutions(
        mut self,
        resolutions: Arc<HashMap<String, LinkResolution>>,
    ) -> Self {
        self.link_resolutions = resolutions;
        self
    }

    /// Opt in to inserting stable-ID link syntax when a candidate has an
    /// unconflicted stable ID. Defaults to ordinary readable wikilinks.
    #[must_use]
    pub fn with_stable_links(mut self, enabled: bool) -> Self {
        self.stable_links = enabled;
        self
    }

    /// Publish acknowledged focus, caret, selection, and composition changes
    /// to the host without copying document text into widget state.
    #[must_use]
    pub fn with_interaction(
        mut self,
        on_interaction: impl Fn(EditorInteractionSnapshot) -> Message + 'a,
    ) -> Self {
        self.on_interaction = Some(Box::new(on_interaction));
        self
    }

    /// Restore host-acknowledged input state after a widget runtime is
    /// recreated or reparented.
    #[must_use]
    pub fn with_initial_interaction(mut self, interaction: EditorInteractionSnapshot) -> Self {
        self.initial_interaction = Some(interaction);
        self
    }

    /// Stable host identity prevents a retained widget slot from inheriting
    /// another view's caret when two views share a buffer.
    #[must_use]
    pub fn with_view_key(mut self, key: u128, incarnation: u64) -> Self {
        self.view_key = Some((key, incarnation));
        self
    }

    /// Modal overlays suppress background editor input without losing its caret.
    #[must_use]
    pub fn with_input_enabled(mut self, enabled: bool) -> Self {
        self.input_enabled = enabled;
        self
    }

    /// Set the requested widget width.
    #[must_use]
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Keep pointer focus available while another control owns keyboard input.
    #[must_use]
    pub fn with_keyboard_enabled(mut self, enabled: bool) -> Self {
        self.keyboard_enabled = enabled;
        self
    }

    /// Capture ordered physical input for an acknowledged host controller.
    /// Existing standalone adapters can retain the guarded edit callback.
    #[must_use]
    pub fn with_input_intents(
        mut self,
        callback: impl Fn(crate::input::CapturedInput) -> Message + 'a,
    ) -> Self {
        self.on_input_intent = Some(Box::new(callback));
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

    /// Headless key dispatch through the real keymap; used by the runtime
    /// event loop and by controlled-input tests.
    pub fn execute_key(
        &self,
        state: &mut EditorState,
        key: &Key,
        modifiers: Modifiers,
        text: Option<&str>,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        if state.autocomplete_start.is_some() {
            match key {
                Key::Named(Named::Escape) => {
                    state.close_autocomplete();
                    return true;
                }
                Key::Named(Named::ArrowUp) => {
                    state.autocomplete_selected = state.autocomplete_selected.saturating_sub(1);
                    return true;
                }
                Key::Named(Named::ArrowDown) => {
                    let count = autocomplete_count(self, state);
                    state.autocomplete_selected = state
                        .autocomplete_selected
                        .saturating_add(1)
                        .min(count.saturating_sub(1));
                    return true;
                }
                Key::Named(Named::Enter) => {
                    let matches = match_candidates(&self.candidates, &state.autocomplete_query, 8);
                    if let Some(candidate) = matches.get(state.autocomplete_selected) {
                        let start = state.autocomplete_start.unwrap_or(state.caret);
                        let inserted = flavite_syntax::link_format::format_candidate_link(
                            &candidate.target,
                            &candidate.title,
                            candidate.stable_id.as_deref(),
                            self.stable_links,
                        );
                        self.emit(
                            shell,
                            EditorAction::replace(&self.view, start, state.caret, inserted.clone()),
                        );
                        state.caret = start + inserted.len();
                        state.anchor = state.caret;
                    } else if state.autocomplete_selected == matches.len()
                        && !state.autocomplete_query.trim().is_empty()
                    {
                        let start = state.autocomplete_start.unwrap_or(state.caret);
                        let target = state.autocomplete_query.trim().to_owned();
                        self.emit(
                            shell,
                            EditorAction::open_link(
                                &self.view,
                                LinkNavigation {
                                    target,
                                    alias: None,
                                    anchor: None,
                                    by_id: false,
                                    span: flavite_syntax::text::Span::new(
                                        flavite_syntax::TextByte(start),
                                        flavite_syntax::TextByte(state.caret),
                                    ),
                                    create_if_unresolved: true,
                                },
                            ),
                        );
                    }
                    state.close_autocomplete();
                    state.ensure_caret_visible(&self.view.text);
                    return true;
                }
                _ => {}
            }
        }
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
                    if action == KeyAction::Insert && replacement == "[" && start == end {
                        if state.caret >= 2
                            && self.view.text.get(state.caret - 2..state.caret - 1) == Some("[")
                        {
                            state.open_autocomplete(state.caret - 2);
                        } else if state.autocomplete_start.is_some() {
                            state.autocomplete_query.push('[');
                            state.autocomplete_selected = 0;
                        }
                    } else if state.autocomplete_start.is_some() && action == KeyAction::Insert {
                        state.autocomplete_query.push_str(replacement);
                        state.autocomplete_selected = 0;
                    }
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
                    KeyAction::Home if modifiers.command() || modifiers.control() => 0,
                    KeyAction::End if modifiers.command() || modifiers.control() => {
                        self.view.text.len()
                    }
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
                    if state.autocomplete_start.is_some() {
                        state.autocomplete_query.pop();
                        state.autocomplete_selected = 0;
                    }
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
    view_key: Option<(u128, u64)>,
    buffer_id: Option<u64>,
    revision: Option<u64>,
    link_spans: Vec<LinkSpan>,
    decorations: Vec<flavite_syntax::ScannedSpan>,
    /// Source line lookup shared by caret mapping and viewport extraction.
    line_starts: Vec<usize>,
    line_text_addr: usize,
    line_text_len: usize,
    resolutions_present: bool,
    caret: usize,
    anchor: usize,
    focused: bool,
    ime_active: bool,
    selecting: bool,
    modifiers: Modifiers,
    autocomplete_start: Option<usize>,
    autocomplete_query: String,
    autocomplete_selected: usize,
    viewport: Viewport,
    composition: Option<Composition>,
    interaction_sequence: u64,
    controlled_pending: bool,
    controlled_ack: u64,
    /// Cached shaping of the visible viewport text (see [`ShapingCache`]).
    shaping: Option<ShapingCache>,
    /// Last interaction-snapshot publication instant (drag coalescing).
    last_publish: Option<Instant>,
    /// The active type settings, synced from the widget in `diff` (W11).
    type_metrics: TypeMetrics,
    /// The caret byte offset at which [`EditorState::ensure_caret_visible`]
    /// last ran. Re-running it every frame would snap the viewport back to
    /// the caret, cancelling wheel/scrollbar scrolling away from it.
    last_ensured_caret: usize,
    /// Live scrollbar-thumb drag, with the pointer's grab offset inside the
    /// thumb. `Some` while the left button holds the thumb.
    scroll_drag: Option<ScrollDrag>,
}

/// One active scrollbar-thumb drag.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ScrollDrag {
    /// Pointer offset from the thumb's top edge when the drag started.
    grab_offset: f32,
}

/// Identity of one shaped viewport: the same revision, text allocation,
/// viewport window, and layout width reuse the cached shaping. Any change
/// reshapes, so hit-testing, IME placement, and drawing share one shaping
/// pass per frame instead of re-shaping on every event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ShapingKey {
    revision: u64,
    text_addr: usize,
    text_len: usize,
    first_line: usize,
    visible_lines: usize,
    width_bits: u32,
    /// Font/line-height bits, so a preference change reshapes immediately.
    type_bits: u64,
}

impl ShapingKey {
    fn new(
        revision: u64,
        text_addr: usize,
        text_len: usize,
        viewport: Viewport,
        width_bits: u32,
        metrics: TypeMetrics,
    ) -> Self {
        Self {
            revision,
            text_addr,
            text_len,
            first_line: viewport.first_line,
            visible_lines: viewport.visible_lines,
            width_bits,
            type_bits: (metrics.font_size.to_bits() as u64) << 32
                | u64::from(metrics.line_height.to_bits()),
        }
    }
}

struct ShapingCache {
    key: ShapingKey,
    buffer: ShapingBuffer,
    visible: String,
    source_start: usize,
}

impl std::fmt::Debug for ShapingCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShapingCache")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Host-visible per-view interaction state needed by accessibility and
/// composition-aware workspace commands. The text itself remains owned by the
/// revisioned buffer view and is not copied into widget state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorInteractionSnapshot {
    pub sequence: u64,
    pub buffer_id: u64,
    pub revision: u64,
    pub caret: usize,
    pub selection: (usize, usize),
    pub viewport_anchor: usize,
    pub focused: bool,
    pub composing: bool,
}

/// Inspect the focused custom editor through iced's normal widget operation
/// traversal. The result is absent when no editor currently owns focus.
pub fn inspect_focused_editor()
-> impl iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>> {
    struct Inspect(Option<EditorInteractionSnapshot>);

    impl iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>> for Inspect {
        fn traverse(
            &mut self,
            operate: &mut dyn FnMut(
                &mut dyn iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>>,
            ),
        ) {
            if self.0.is_none() {
                operate(self);
            }
        }

        fn custom(
            &mut self,
            _id: Option<&iced::advanced::widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            let Some(state) = state.downcast_ref::<EditorState>() else {
                return;
            };
            if !state.focused {
                return;
            }
            let Some(buffer_id) = state.buffer_id else {
                return;
            };
            let Some(revision) = state.revision else {
                return;
            };
            self.0 = Some(EditorInteractionSnapshot {
                sequence: state.interaction_sequence,
                buffer_id,
                revision,
                caret: state.caret,
                selection: state.selection(),
                viewport_anchor: 0,
                focused: true,
                composing: state.is_composing(),
            });
        }

        fn finish(
            &self,
        ) -> iced::advanced::widget::operation::Outcome<Option<EditorInteractionSnapshot>> {
            iced::advanced::widget::operation::Outcome::Some(self.0)
        }
    }

    Inspect(None)
}

/// Set the focused editor's selection through iced's normal widget operation
/// path. The byte offsets are validated by the host against the current text
/// snapshot before this operation is dispatched.
pub fn set_focused_editor_selection(
    expected_buffer_id: u64,
    expected_revision: u64,
    anchor: usize,
    focus: usize,
) -> impl iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>> {
    struct SetSelection {
        expected_buffer_id: u64,
        expected_revision: u64,
        anchor: usize,
        focus: usize,
        result: Option<EditorInteractionSnapshot>,
    }

    impl iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>> for SetSelection {
        fn traverse(
            &mut self,
            operate: &mut dyn FnMut(
                &mut dyn iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>>,
            ),
        ) {
            if self.result.is_none() {
                operate(self);
            }
        }

        fn custom(
            &mut self,
            _id: Option<&iced::advanced::widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            let Some(state) = state.downcast_mut::<EditorState>() else {
                return;
            };
            if state.is_composing() {
                return;
            }
            if state.buffer_id != Some(self.expected_buffer_id)
                || state.revision != Some(self.expected_revision)
            {
                return;
            }
            state.anchor = self.anchor;
            state.caret = self.focus;
            state.focused = true;
            state.ime_active = true;
            self.result = state.interaction_snapshot(None);
        }

        fn finish(
            &self,
        ) -> iced::advanced::widget::operation::Outcome<Option<EditorInteractionSnapshot>> {
            iced::advanced::widget::operation::Outcome::Some(self.result)
        }
    }

    SetSelection {
        expected_buffer_id,
        expected_revision,
        anchor,
        focus,
        result: None,
    }
}

/// Focus the matching editor instance through iced's widget operation path.
/// The host captures the buffer incarnation as a revision pair so a late
/// native action cannot focus a replacement note in the same widget slot.
pub fn focus_editor(
    expected_buffer_id: u64,
    expected_revision: u64,
) -> impl iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>> {
    struct FocusEditor {
        expected_buffer_id: u64,
        expected_revision: u64,
        result: Option<EditorInteractionSnapshot>,
    }

    impl iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>> for FocusEditor {
        fn traverse(
            &mut self,
            operate: &mut dyn FnMut(
                &mut dyn iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>>,
            ),
        ) {
            if self.result.is_none() {
                operate(self);
            }
        }

        fn custom(
            &mut self,
            _id: Option<&iced::advanced::widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            let Some(state) = state.downcast_mut::<EditorState>() else {
                return;
            };
            if state.is_composing()
                || state.buffer_id != Some(self.expected_buffer_id)
                || state.revision != Some(self.expected_revision)
            {
                return;
            }
            state.focused = true;
            state.ime_active = true;
            self.result = state.interaction_snapshot(None);
        }

        fn finish(
            &self,
        ) -> iced::advanced::widget::operation::Outcome<Option<EditorInteractionSnapshot>> {
            iced::advanced::widget::operation::Outcome::Some(self.result)
        }
    }

    FocusEditor {
        expected_buffer_id,
        expected_revision,
        result: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Composition {
    start: usize,
    end: usize,
    preedit: String,
    selection: Option<std::ops::Range<usize>>,
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

    /// Whether an IME owns the current input sequence. Hosts use this to
    /// refuse actions that would hide or replace a composing editor.
    #[must_use]
    /// Whether this editor currently owns keyboard focus.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn is_composing(&self) -> bool {
        self.composition.is_some()
    }

    fn interaction_snapshot(&self, text: Option<&str>) -> Option<EditorInteractionSnapshot> {
        Some(EditorInteractionSnapshot {
            sequence: self.interaction_sequence,
            buffer_id: self.buffer_id?,
            revision: self.revision?,
            caret: self.caret,
            selection: self.selection(),
            viewport_anchor: text.map_or(0, |text| line_start_byte(text, self.viewport.first_line)),
            focused: self.focused,
            composing: self.is_composing(),
        })
    }

    /// Install a host-controlled interaction acknowledgement. Lower or
    /// equal sequences and mismatched buffer/revision are rejected, so a
    /// delayed or stale acknowledgement can never move the caret back.
    pub fn restore_interaction(&mut self, text: &str, interaction: EditorInteractionSnapshot) {
        if self.buffer_id != Some(interaction.buffer_id)
            || self.revision != Some(interaction.revision)
            || interaction.sequence <= self.interaction_sequence
        {
            return;
        }
        self.anchor = clamp_char_boundary(text, interaction.selection.0);
        self.caret = clamp_char_boundary(text, interaction.selection.1);
        self.viewport.first_line = text[..interaction.viewport_anchor.min(text.len())]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        self.focused = interaction.focused;
        self.ime_active = interaction.focused;
        self.interaction_sequence = interaction.sequence;
        if self.focused {
            self.ensure_caret_visible(text);
        }
        self.last_ensured_caret = self.caret;
    }

    /// Feed one platform input-method event through the controlled
    /// composition state; preedit stays transient and commits to at most
    /// one revision-guarded edit.
    pub fn input_method_event(
        &mut self,
        view: &EditorView,
        event: &iced::advanced::input_method::Event,
    ) -> Option<EditorAction> {
        match event {
            input_method::Event::Opened => {
                self.ime_active = true;
                None
            }
            input_method::Event::Preedit(preedit, selection) => {
                if preedit.is_empty() {
                    // winit sends an empty preedit immediately before commit.
                    // Keep an existing transaction alive, but an opened IME
                    // with no preedit is not itself active composition and
                    // must not suppress ordinary key input.
                    if let Some(composition) = &mut self.composition {
                        composition.preedit.clear();
                        composition.selection.clone_from(selection);
                    }
                    return None;
                }
                let (start, end) = self.selection();
                let composition = self.composition.get_or_insert_with(|| Composition {
                    start,
                    end,
                    preedit: String::new(),
                    selection: None,
                });
                composition.preedit.clone_from(preedit);
                composition.selection.clone_from(selection);
                None
            }
            input_method::Event::Commit(committed) => {
                let composition = self.composition.take();
                let (start, end) = composition
                    .map(|composition| (composition.start, composition.end))
                    .unwrap_or_else(|| self.selection());
                if committed.is_empty() {
                    return None;
                }
                self.caret = start + committed.len();
                self.anchor = self.caret;
                self.ensure_caret_visible(&view.text);
                Some(EditorAction::replace(view, start, end, committed.clone()))
            }
            input_method::Event::Closed => {
                self.composition = None;
                // `Closed` ends this composition session, not editor focus.
                // Keep requesting an input method while focused so switching
                // back to an IME can open a new composition without requiring
                // an extra click into the editor.
                None
            }
        }
    }

    fn open_autocomplete(&mut self, start: usize) {
        self.autocomplete_start = Some(start);
        self.autocomplete_query.clear();
        self.autocomplete_selected = 0;
    }

    fn close_autocomplete(&mut self) {
        self.autocomplete_start = None;
        self.autocomplete_query.clear();
        self.autocomplete_selected = 0;
    }

    fn move_to(&mut self, byte: usize, extend: bool) {
        self.caret = byte;
        if !extend {
            self.anchor = byte;
        }
    }

    /// Reconcile widget state with a replacement controlled buffer view
    /// (reload, journal patch, reopen under the same tab role).
    pub fn reconcile(
        &mut self,
        view: &EditorView,
        link_resolutions: &HashMap<String, LinkResolution>,
    ) {
        if self
            .buffer_id
            .is_some_and(|buffer_id| buffer_id != view.buffer_id)
        {
            *self = Self::default();
        }
        self.buffer_id = Some(view.buffer_id);
        if self.revision != Some(view.revision) {
            let (link_spans, decorations) = scan_note(&view.text);
            self.link_spans = link_spans;
            self.decorations = decorations;
            self.line_starts.clear();
            self.line_starts.push(0);
            self.line_starts
                .extend(view.text.match_indices('\n').map(|(byte, _)| byte + 1));
            self.line_text_addr = view.text.as_ptr() as usize;
            self.line_text_len = view.text.len();
            self.revision = Some(view.revision);
        }
        if !link_resolutions.is_empty() || self.resolutions_present {
            for span in &mut self.link_spans {
                let key = flavite_syntax::text::fold_key(&span.payload.target_raw);
                span.resolution = resolve_link(span, link_resolutions.get(&key).copied());
            }
        }
        self.resolutions_present = !link_resolutions.is_empty();
        self.caret = clamp_char_boundary(&view.text, self.caret);
        self.anchor = clamp_char_boundary(&view.text, self.anchor);
        self.viewport.line_height = self.type_metrics.line_height;
        self.viewport.first_line = self
            .viewport
            .first_line
            .min(self.source_line_count(&view.text).saturating_sub(1));
        // Only re-anchor the viewport when the caret moved. Running this
        // every frame would snap the view back to the caret and cancel any
        // wheel or scrollbar scroll away from it.
        if self.caret != self.last_ensured_caret {
            self.ensure_caret_visible(&view.text);
            self.last_ensured_caret = self.caret;
        }
    }

    fn ensure_caret_visible(&mut self, text: &str) {
        let caret = self.caret.min(text.len());
        let line = if self.has_line_index(text) {
            self.line_starts
                .partition_point(|start| *start <= caret)
                .saturating_sub(1)
        } else {
            text[..caret].bytes().filter(|byte| *byte == b'\n').count()
        };
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

    fn has_line_index(&self, text: &str) -> bool {
        !self.line_starts.is_empty()
            && self.line_text_addr == text.as_ptr() as usize
            && self.line_text_len == text.len()
    }

    fn source_line_count(&self, text: &str) -> usize {
        if self.has_line_index(text) {
            self.line_starts.len()
        } else {
            logical_line_count(text)
        }
    }

    fn visible_text(&self, text: &str, viewport: Viewport) -> (String, usize) {
        if !self.has_line_index(text) {
            return visible_text(text, viewport);
        }
        let first = viewport.first_line.min(self.line_starts.len() - 1);
        let end_line = first.saturating_add(viewport.visible_lines.max(1));
        let start = self.line_starts[first];
        let end = self
            .line_starts
            .get(end_line)
            .copied()
            .unwrap_or(text.len());
        (text[start..end].to_owned(), start)
    }

    /// Pointer selection against shaped text: places or extends the
    /// selection at the glyph boundary under the point.
    pub fn place_caret_from_point(
        &mut self,
        text: &str,
        revision: u64,
        x: f32,
        y: f32,
        width: f32,
        extend: bool,
    ) {
        let byte = self.hit_test_cached(text, revision, self.viewport, x, y, width);
        self.move_to(byte, extend);
    }

    /// Shape the visible viewport once per (revision, text, viewport, width)
    /// and reuse the cached buffer for every consumer in the same frame.
    fn shaped(
        &mut self,
        text: &str,
        revision: u64,
        viewport: Viewport,
        width: f32,
    ) -> (&ShapingBuffer, &str, usize) {
        let key = ShapingKey::new(
            revision,
            text.as_ptr() as usize,
            text.len(),
            viewport,
            width.to_bits(),
            self.type_metrics,
        );
        if self.shaping.as_ref().is_none_or(|cache| cache.key != key) {
            let (visible, source_start) = self.visible_text(text, viewport);
            let buffer = shape_text(&visible, width, self.type_metrics);
            self.shaping = Some(ShapingCache {
                key,
                buffer,
                visible,
                source_start,
            });
        }
        let cache = self
            .shaping
            .as_ref()
            .expect("shaping cache was just populated");
        (&cache.buffer, cache.visible.as_str(), cache.source_start)
    }

    /// Hit-test against the cached viewport shaping.
    fn hit_test_cached(
        &mut self,
        text: &str,
        revision: u64,
        viewport: Viewport,
        x: f32,
        y: f32,
        width: f32,
    ) -> usize {
        {
            let (buffer, visible, source_start) = self.shaped(text, revision, viewport, width);
            let Some(cursor) = buffer.hit(x.max(0.0), y.max(0.0)) else {
                return source_start + visible.len();
            };
            let line_start = line_start_byte(visible, cursor.line);
            source_start
                + clamp_char_boundary(visible, line_start + cursor.index).min(visible.len())
        }
    }
}

impl<Message: 'static> Widget<Message, Theme, iced::Renderer> for EditorWidget<'_, Message> {
    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    fn layout(&mut self, tree: &mut Tree, _renderer: &iced::Renderer, limits: &Limits) -> Node {
        let size = limits.resolve(self.width, self.height, Size::new(640.0, 480.0));
        let state = tree.state.downcast_mut::<EditorState>();
        state.viewport.set_height(size.height - PAD_Y * 2.0);
        state.shaped(
            &self.view.text,
            self.view.revision,
            state.viewport,
            self.text_wrap_width(size.width),
        );
        Node::new(size)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
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
            Background::Color(theme.palette().background),
        );
        let clip = bounds
            .intersection(viewport)
            .unwrap_or(Rectangle::with_size(Size::ZERO));
        let text_width = self.text_wrap_width(bounds.width);
        let shaping_key = ShapingKey::new(
            self.view.revision,
            self.view.text.as_ptr() as usize,
            self.view.text.len(),
            state.viewport,
            text_width.to_bits(),
            self.type_metrics,
        );
        let local_buffer;
        let hoisted_visible;
        let (visible_text, source_start, runs) = if let Some(cache) = state
            .shaping
            .as_ref()
            .filter(|cache| cache.key == shaping_key)
        {
            let runs = cache.buffer.layout_runs().collect::<Vec<_>>();
            (cache.visible.as_str(), cache.source_start, runs)
        } else {
            let (visible, start) = visible_text(&self.view.text, state.viewport);
            hoisted_visible = visible;
            local_buffer = shape_text(&hoisted_visible, text_width, self.type_metrics);
            let runs = local_buffer.layout_runs().collect::<Vec<_>>();
            (hoisted_visible.as_str(), start, runs)
        };
        let (selection_start, selection_end) = state.selection();
        if selection_start < selection_end {
            for run in &runs {
                let line_start = source_start + line_start_byte(visible_text, run.line_i);
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
                                    Size::new(glyph.w.max(1.0), self.type_metrics.line_height),
                                ),
                                border: Border::default(),
                                ..renderer::Quad::default()
                            },
                            Background::Color(self.colors.selection),
                        );
                    }
                }
            }
        }
        // Use the semantic scanner for link decorations. No markdown parser or
        // second link grammar is embedded in the widget. Spans intersecting
        // each line are collected once per line, not once per glyph.
        for run in &runs {
            let line_start = source_start + line_start_byte(visible_text, run.line_i);
            let line_end = line_bounds(&self.view.text, line_start).1;
            let active_links: Vec<&LinkSpan> = state
                .link_spans
                .iter()
                .filter(|span| span.span.start < line_end && span.span.end > line_start)
                .collect();
            let active_decorations: Vec<&flavite_syntax::ScannedSpan> = state
                .decorations
                .iter()
                .filter(|span| {
                    matches!(span.kind, SpanKind::Tag | SpanKind::BlockId)
                        && span.span.start.0 < line_end
                        && span.span.end.0 > line_start
                })
                .collect();
            for glyph in run.glyphs {
                let glyph_start = line_start + glyph.start;
                let glyph_end = line_start + glyph.end;
                let link_resolution = active_links
                    .iter()
                    .find(|span| span.span.start < glyph_end && span.span.end > glyph_start)
                    .map(|span| span.resolution);
                let other_decoration = active_decorations
                    .iter()
                    .any(|span| span.span.start.0 < glyph_end && span.span.end.0 > glyph_start);
                if link_resolution.is_some() || other_decoration {
                    let color = match link_resolution {
                        Some(LinkResolution::Unresolved) => self.colors.link_unresolved,
                        Some(LinkResolution::MultipleCandidates) => self.colors.link_multiple,
                        _ => self.colors.link,
                    };
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle::new(
                                Point::new(
                                    bounds.x + PAD_X + glyph.x,
                                    bounds.y + PAD_Y + run.line_top + self.type_metrics.line_height
                                        - 2.0,
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
        renderer.fill_text(
            text::Text {
                content: visible_text.to_owned(),
                bounds: Size::new(
                    (bounds.width - PAD_X * 2.0).max(1.0),
                    (bounds.height - PAD_Y * 2.0).max(1.0),
                ),
                size: iced::Pixels(self.type_metrics.font_size),
                line_height: text::LineHeight::Absolute(iced::Pixels(
                    self.type_metrics.line_height,
                )),
                font: iced::Font::MONOSPACE,
                align_x: text::Alignment::Left,
                align_y: iced::alignment::Vertical::Top,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::WordOrGlyph,
            },
            Point::new(bounds.x + PAD_X, bounds.y + PAD_Y),
            self.colors.text,
            clip,
        );
        // Scrollbar: only when the document overflows the viewport. The
        // wheel and gutter-drag share this geometry.
        let total_lines = state.source_line_count(&self.view.text);
        if total_lines > state.viewport.visible_lines.max(1) {
            let (track_y, track_height, thumb_y, thumb_height, _max_first) = scrollbar_geometry(
                bounds.height,
                total_lines,
                state.viewport.visible_lines.max(1),
                state.viewport.first_line,
            );
            let text = self.colors.text;
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(
                            bounds.x + bounds.width - SCROLLBAR_GUTTER,
                            bounds.y + track_y,
                        ),
                        Size::new(SCROLLBAR_GUTTER, track_height),
                    ),
                    border: Border::default(),
                    ..renderer::Quad::default()
                },
                Background::Color(Color::from_rgba(text.r, text.g, text.b, 0.06)),
            );
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(
                            bounds.x + bounds.width - SCROLLBAR_GUTTER
                                + (SCROLLBAR_GUTTER - SCROLLBAR_WIDTH) / 2.0,
                            bounds.y + thumb_y,
                        ),
                        Size::new(SCROLLBAR_WIDTH, thumb_height),
                    ),
                    border: Border::default(),
                    ..renderer::Quad::default()
                },
                Background::Color(Color::from_rgba(
                    text.r,
                    text.g,
                    text.b,
                    if state.scroll_drag.is_some() {
                        0.75
                    } else {
                        0.45
                    },
                )),
            );
        }
        if let Some(composition) = &state.composition
            && !composition.preedit.is_empty()
            && composition.start >= source_start
            && composition.start <= source_start + visible_text.len()
        {
            let local_start = composition.start - source_start;
            let (x, y) = caret_position(&runs, visible_text, local_start);
            let preedit_layout = shape_text(
                &composition.preedit,
                self.text_wrap_width(bounds.width),
                self.type_metrics,
            );
            let preedit_width = preedit_layout
                .layout_runs()
                .map(|run| run.glyphs.iter().map(|glyph| glyph.w).sum::<f32>())
                .fold(0.0_f32, f32::max);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(
                            bounds.x + PAD_X + x,
                            bounds.y + PAD_Y + y + self.type_metrics.line_height - 2.0,
                        ),
                        Size::new(preedit_width.max(1.0), 1.5),
                    ),
                    border: Border::default(),
                    ..renderer::Quad::default()
                },
                Background::Color(self.colors.preedit_underline),
            );
            renderer.fill_text(
                text::Text {
                    content: composition.preedit.clone(),
                    bounds: Size::new(
                        (bounds.width - PAD_X * 2.0).max(1.0),
                        self.type_metrics.line_height,
                    ),
                    size: iced::Pixels(self.type_metrics.font_size),
                    line_height: text::LineHeight::Absolute(iced::Pixels(
                        self.type_metrics.line_height,
                    )),
                    font: iced::Font::MONOSPACE,
                    align_x: text::Alignment::Left,
                    align_y: iced::alignment::Vertical::Top,
                    shaping: text::Shaping::Advanced,
                    wrapping: text::Wrapping::None,
                },
                Point::new(bounds.x + PAD_X + x, bounds.y + PAD_Y + y),
                self.colors.preedit_text,
                clip,
            );
        }
        if state.autocomplete_start.is_some() {
            let matches = match_candidates(&self.candidates, &state.autocomplete_query, 8);
            let has_create = !state.autocomplete_query.trim().is_empty();
            if !matches.is_empty() || has_create {
                let row_count = matches.len() + usize::from(has_create);
                let line = self.view.text[..state.caret.min(self.view.text.len())]
                    .matches('\n')
                    .count();
                let popup = Rectangle::new(
                    Point::new(
                        bounds.x + PAD_X,
                        bounds.y
                            + PAD_Y
                            + (line.saturating_sub(state.viewport.first_line) as f32 + 1.0)
                                * self.type_metrics.line_height,
                    ),
                    Size::new(320.0, row_count as f32 * 23.0 + 8.0),
                );
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: popup,
                        border: Border::default(),
                        ..renderer::Quad::default()
                    },
                    Background::Color(self.colors.autocomplete_background),
                );
                for (index, candidate) in matches.iter().enumerate() {
                    let color = if index == state.autocomplete_selected {
                        self.colors.autocomplete_selected
                    } else {
                        self.colors.autocomplete_text
                    };
                    renderer.fill_text(
                        text::Text {
                            content: format!("{}  ·  {}", candidate.title, candidate.target),
                            bounds: Size::new(304.0, 22.0),
                            size: iced::Pixels(14.0),
                            line_height: text::LineHeight::Absolute(iced::Pixels(18.0)),
                            font: iced::Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Center,
                            shaping: text::Shaping::Advanced,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(popup.x + 8.0, popup.y + 4.0 + index as f32 * 23.0),
                        color,
                        clip,
                    );
                }
                if has_create {
                    let index = matches.len();
                    let color = if index == state.autocomplete_selected {
                        self.colors.autocomplete_selected
                    } else {
                        self.colors.autocomplete_text
                    };
                    renderer.fill_text(
                        text::Text {
                            content: format!("Create note: {}", state.autocomplete_query.trim()),
                            bounds: Size::new(304.0, 22.0),
                            size: iced::Pixels(14.0),
                            line_height: text::LineHeight::Absolute(iced::Pixels(18.0)),
                            font: iced::Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Center,
                            shaping: text::Shaping::Advanced,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(popup.x + 8.0, popup.y + 4.0 + index as f32 * 23.0),
                        color,
                        clip,
                    );
                }
            }
        }
        if state.focused
            && state.caret >= source_start
            && state.caret <= source_start + visible_text.len()
        {
            let local_caret = state.caret.saturating_sub(source_start);
            let (x, y) = caret_position(&runs, visible_text, local_caret);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(bounds.x + PAD_X + x, bounds.y + PAD_Y + y),
                        Size::new(1.5, self.type_metrics.line_height),
                    ),
                    border: Border::default(),
                    ..renderer::Quad::default()
                },
                Background::Color(self.colors.autocomplete_accent),
            );
        }
    }

    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<EditorState>()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        let mut state = EditorState {
            view_key: self.view_key,
            ..EditorState::default()
        };
        state.reconcile(&self.view, &self.link_resolutions);
        if let Some(interaction) = self.initial_interaction {
            if self.on_input_intent.is_some()
                && state.controlled_pending
                && interaction.sequence > state.controlled_ack
            {
                let captured_sequence = state.interaction_sequence;
                state.interaction_sequence = interaction.sequence.saturating_sub(1);
                state.restore_interaction(&self.view.text, interaction);
                state.interaction_sequence = state.interaction_sequence.max(captured_sequence);
                state.controlled_ack = interaction.sequence;
                state.controlled_pending = interaction.sequence < captured_sequence;
                if self
                    .view
                    .text
                    .get(..state.caret)
                    .is_some_and(|text| text.ends_with("[["))
                {
                    state.open_autocomplete(state.caret - 2);
                }
                if let Some(start) = state.autocomplete_start
                    && let Some(query) = self.view.text.get(start + 2..state.caret)
                {
                    state.autocomplete_query = query.into();
                }
            } else {
                state.restore_interaction(&self.view.text, interaction);
            }
        }
        iced::advanced::widget::tree::State::new(state)
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<EditorState>();
        // Host type preferences (W11) apply on the next frame; dropping the
        // shaping cache forces one reshape with the new metrics.
        if state.type_metrics != self.type_metrics {
            state.type_metrics = self.type_metrics;
            state.viewport.line_height = self.type_metrics.line_height;
            state.shaping = None;
        }
        if state.view_key != self.view_key {
            *state = EditorState::default();
            state.view_key = self.view_key;
        }
        state.reconcile(&self.view, &self.link_resolutions);
        if let Some(interaction) = self.initial_interaction {
            if self.on_input_intent.is_some()
                && state.controlled_pending
                && interaction.sequence > state.controlled_ack
            {
                let captured_sequence = state.interaction_sequence;
                state.interaction_sequence = interaction.sequence.saturating_sub(1);
                state.restore_interaction(&self.view.text, interaction);
                state.interaction_sequence = state.interaction_sequence.max(captured_sequence);
                state.controlled_ack = interaction.sequence;
                state.controlled_pending = interaction.sequence < captured_sequence;
                if self
                    .view
                    .text
                    .get(..state.caret)
                    .is_some_and(|text| text.ends_with("[["))
                {
                    state.open_autocomplete(state.caret - 2);
                }
                if let Some(start) = state.autocomplete_start
                    && let Some(query) = self.view.text.get(start + 2..state.caret)
                {
                    state.autocomplete_query = query.into();
                }
            } else {
                state.restore_interaction(&self.view.text, interaction);
            }
        }
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        operation.custom(
            None,
            layout.bounds(),
            tree.state.downcast_mut::<EditorState>(),
        );
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
        // Modal overlays suppress keyboard and IME edit input only; pointer
        // hit-testing, selection, and focus stay live so an open overlay or
        // stale overlay flag never leaves the editor dead to the mouse.
        if keyboard_input_blocked(self.input_enabled, self.keyboard_enabled, event) {
            return;
        }
        let before_interaction = state.interaction_snapshot(Some(&self.view.text));
        if let iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key,
            modifiers,
            text,
            ..
        }) = event
            && state.focused
            && state.composition.is_none()
            && let Some(callback) = &self.on_input_intent
        {
            let action = key_action(key, *modifiers, text.as_deref());
            let completion_control = state.autocomplete_start.is_some()
                && matches!(
                    key,
                    Key::Named(Named::Enter | Named::ArrowUp | Named::ArrowDown | Named::Escape)
                );
            if action != KeyAction::Ignore && !completion_control {
                let committed = if action == KeyAction::Paste {
                    clipboard
                        .read(iced::advanced::clipboard::Kind::Standard)
                        .unwrap_or_default()
                } else {
                    text.clone().unwrap_or_default().to_string()
                };
                state.interaction_sequence = state.interaction_sequence.saturating_add(1);
                state.controlled_pending = true;
                shell.publish(callback(crate::input::CapturedInput {
                    buffer: self.view.buffer_id,
                    observed_revision: self.view.revision,
                    sequence: state.interaction_sequence,
                    action,
                    text: committed,
                    extend: modifiers.shift(),
                    document_boundary: modifiers.command() || modifiers.control(),
                }));
                shell.capture_event();
                shell.request_redraw();
                return;
            }
        }
        match event {
            iced::Event::InputMethod(input_event) if state.focused => {
                if let Some(action) = state.input_method_event(&self.view, input_event) {
                    self.emit(shell, action);
                }
                shell.capture_event();
                shell.request_redraw();
            }
            iced::Event::Window(iced::window::Event::Unfocused) => {
                state.focused = false;
                state.ime_active = false;
                state.composition = None;
                shell.request_input_method::<&str>(&InputMethod::Disabled);
            }
            iced::Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(position) = cursor.position_in(layout.bounds()) {
                    let bounds = layout.bounds();
                    let total_lines = state.source_line_count(&self.view.text);
                    let visible_lines = state.viewport.visible_lines.max(1);
                    if total_lines > visible_lines && position.x >= bounds.width - SCROLLBAR_GUTTER
                    {
                        // A press in the scrollbar gutter grabs the thumb (or
                        // jumps to the pressed position) and never starts a
                        // text selection.
                        let (track_y, track_height, thumb_y, thumb_height, max_first) =
                            scrollbar_geometry(
                                bounds.height,
                                total_lines,
                                visible_lines,
                                state.viewport.first_line,
                            );
                        let local_y = (position.y - bounds.y - track_y).clamp(0.0, track_height);
                        let thumb_top = thumb_y - track_y;
                        if local_y >= thumb_top && local_y <= thumb_top + thumb_height {
                            state.scroll_drag = Some(ScrollDrag {
                                grab_offset: local_y - thumb_top,
                            });
                        } else {
                            let ratio = ((local_y - thumb_height / 2.0)
                                / (track_height - thumb_height).max(1.0))
                            .clamp(0.0, 1.0);
                            state.viewport.first_line = (ratio * max_first as f32).round() as usize;
                            state.scroll_drag = Some(ScrollDrag {
                                grab_offset: thumb_height / 2.0,
                            });
                        }
                        state.focused = true;
                        state.ime_active = true;
                    } else {
                        let width = self.text_wrap_width(layout.bounds().width);
                        let byte = state.hit_test_cached(
                            &self.view.text,
                            self.view.revision,
                            state.viewport,
                            position.x - PAD_X,
                            position.y - PAD_Y,
                            width,
                        );
                        // A click on a link navigates (point-and-click contract);
                        // modifier clicks take the same path.
                        if let Some(action) = link_action_at(&self.view, &state.link_spans, byte) {
                            self.emit(shell, action);
                            shell.capture_event();
                            shell.request_redraw();
                            return;
                        }
                        state.focused = true;
                        state.ime_active = true;
                        state.selecting = true;
                        state.place_caret_from_point(
                            &self.view.text,
                            self.view.revision,
                            position.x - PAD_X,
                            position.y - PAD_Y,
                            width,
                            false,
                        );
                    }
                    shell.capture_event();
                    shell.request_redraw();
                } else {
                    state.focused = false;
                    state.ime_active = false;
                    state.selecting = false;
                    state.composition = None;
                }
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.scroll_drag.is_some() => {
                if let (Some(position), Some(drag)) =
                    (cursor.position_in(layout.bounds()), state.scroll_drag)
                {
                    let bounds = layout.bounds();
                    let total_lines = state.source_line_count(&self.view.text);
                    let visible_lines = state.viewport.visible_lines.max(1);
                    if total_lines > visible_lines {
                        let (track_y, track_height, _thumb_y, thumb_height, max_first) =
                            scrollbar_geometry(
                                bounds.height,
                                total_lines,
                                visible_lines,
                                state.viewport.first_line,
                            );
                        let travel = (track_height - thumb_height).max(1.0);
                        let local_y =
                            (position.y - bounds.y - track_y - drag.grab_offset).clamp(0.0, travel);
                        let target = (local_y / travel * max_first as f32).round() as usize;
                        if target != state.viewport.first_line {
                            state.viewport.first_line = target;
                            shell.request_redraw();
                        }
                    }
                }
                shell.capture_event();
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.selecting => {
                if let Some(position) = cursor.position_in(layout.bounds()) {
                    state.place_caret_from_point(
                        &self.view.text,
                        self.view.revision,
                        position.x - PAD_X,
                        position.y - PAD_Y,
                        self.text_wrap_width(layout.bounds().width),
                        true,
                    );
                    shell.capture_event();
                    // The selection highlight must be visible on the next
                    // frame, not only after the host acknowledgement round
                    // trip.
                    shell.request_redraw();
                }
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.selecting {
                    shell.capture_event();
                }
                state.selecting = false;
                if state.scroll_drag.take().is_some() {
                    shell.capture_event();
                    shell.request_redraw();
                }
            }
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta })
                if cursor.position_in(layout.bounds()).is_some() =>
            {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / self.type_metrics.line_height,
                };
                let total_lines = state.source_line_count(&self.view.text);
                let visible_lines = state.viewport.visible_lines.max(1);
                // The viewport stays full: the last page bounds the scroll,
                // matching the scrollbar thumb's travel. Wheel-up arrives as
                // a positive y delta and scrolls toward the document start.
                let max_first = total_lines.saturating_sub(visible_lines.min(total_lines));
                if lines > 0.0 {
                    state.viewport.first_line = state
                        .viewport
                        .first_line
                        .saturating_sub(lines.ceil() as usize);
                } else {
                    state.viewport.first_line = state
                        .viewport
                        .first_line
                        .saturating_add(lines.abs().ceil() as usize)
                        .min(max_first);
                }
                shell.capture_event();
                shell.request_redraw();
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(_)) => {}
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) if state.focused
                && state.composition.is_none()
                && self.execute_key(state, key, *modifiers, text.as_deref(), clipboard, shell) =>
            {
                shell.capture_event();
            }
            _ => {}
        }
        if self.input_enabled && state.focused && state.ime_active {
            let width = self.text_wrap_width(layout.bounds().width);
            let caret_byte = state.caret;
            let (x, y) = {
                let (buffer, visible, source_start) =
                    state.shaped(&self.view.text, self.view.revision, state.viewport, width);
                let runs = buffer.layout_runs().collect::<Vec<_>>();
                let caret = caret_byte.saturating_sub(source_start).min(visible.len());
                caret_position(&runs, visible, caret)
            };
            shell.request_input_method(&InputMethod::Enabled {
                cursor: Rectangle::new(
                    Point::new(layout.bounds().x + PAD_X + x, layout.bounds().y + PAD_Y + y),
                    Size::new(1.0, self.type_metrics.line_height),
                ),
                purpose: input_method::Purpose::Normal,
                // The widget paints preedit inline itself. Sending a runtime
                // preedit as well would draw a second over-the-spot copy.
                preedit: None::<input_method::Preedit<&str>>,
            });
        } else {
            shell.request_input_method::<&str>(&InputMethod::Disabled);
        }
        let explicit_interaction = matches!(
            event,
            iced::Event::Mouse(
                mouse::Event::ButtonPressed(mouse::Button::Left)
                    | mouse::Event::ButtonReleased(mouse::Button::Left)
                    | mouse::Event::WheelScrolled { .. }
            ) | iced::Event::Keyboard(keyboard::Event::KeyPressed { .. })
        );
        let publish_due = !state.selecting
            || state
                .last_publish
                .is_none_or(|instant| instant.elapsed() >= INTERACTION_PUBLISH_INTERVAL);
        if let Some(on_interaction) = &self.on_interaction
            && (explicit_interaction
                || state.interaction_snapshot(Some(&self.view.text)) != before_interaction)
            && publish_due
            && let Some(snapshot) = {
                state.interaction_sequence = state.interaction_sequence.saturating_add(1);
                state.interaction_snapshot(Some(&self.view.text))
            }
        {
            state.last_publish = Some(Instant::now());
            shell.publish(on_interaction(snapshot));
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
            if cursor
                .position_in(layout.bounds())
                .is_some_and(|position| position.x >= layout.bounds().width - SCROLLBAR_GUTTER)
            {
                mouse::Interaction::Grabbing
            } else {
                mouse::Interaction::Text
            }
        } else {
            mouse::Interaction::default()
        }
    }
}

fn autocomplete_count<Message>(widget: &EditorWidget<'_, Message>, state: &EditorState) -> usize {
    match_candidates(&widget.candidates, &state.autocomplete_query, 8).len()
        + usize::from(!state.autocomplete_query.trim().is_empty())
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
            Key::Named(Named::Home) => KeyAction::Home,
            Key::Named(Named::End) => KeyAction::End,
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

fn link_action_at(view: &EditorView, spans: &[LinkSpan], byte: usize) -> Option<EditorAction> {
    let link = spans
        .iter()
        .find(|span| !span.embed && span.span.start <= byte && byte < span.span.end)?;
    Some(EditorAction::open_link(
        view,
        LinkNavigation {
            target: link.payload.target_raw.clone(),
            alias: link.payload.alias.clone(),
            anchor: link.payload.anchor.clone(),
            by_id: link.payload.by_id,
            span: flavite_syntax::text::Span::new(
                flavite_syntax::TextByte(link.span.start),
                flavite_syntax::TextByte(link.span.end),
            ),
            create_if_unresolved: true,
        },
    ))
}

/// Shape text at a fixed monospace width for headless pointer-selection
/// tests and glyph-boundary assertions.
#[doc(hidden)]
#[must_use]
pub fn shape_for_test(text: &str, width: f32) -> ShapingBuffer {
    shape_text(text, width, TypeMetrics::default())
}

fn shape_text(text: &str, width: f32, metrics: TypeMetrics) -> ShapingBuffer {
    FONT_SYSTEM.with(|font_system| {
        let mut font_system = font_system.borrow_mut();
        let mut buffer = ShapingBuffer::new(
            &mut font_system,
            Metrics::new(metrics.font_size, metrics.line_height),
        );
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

/// Whether this event is edit-capable keyboard/IME input that the current
/// gating flags suppress. Both flags gate only keyboard and IME events;
/// pointer events are never blocked, so an open overlay (or a stale overlay
/// flag) can never leave the editor dead to mouse selection.
fn keyboard_input_blocked(
    input_enabled: bool,
    keyboard_enabled: bool,
    event: &iced::Event,
) -> bool {
    matches!(
        event,
        iced::Event::Keyboard(keyboard::Event::KeyPressed { .. }) | iced::Event::InputMethod(_)
    ) && (!input_enabled || !keyboard_enabled)
}

/// Shape and hit-test only the logical lines currently in the viewport.
/// Test-only reference implementation; the widget path goes through the
/// cached shaping in [`EditorState::shaped`].
#[cfg(test)]
fn hit_test_visible(text: &str, viewport: Viewport, x: f32, y: f32, width: f32) -> usize {
    let (visible, source_start) = visible_text(text, viewport);
    let buffer = shape_text(&visible, width, TypeMetrics::default());
    let Some(cursor) = buffer.hit(x.max(0.0), y.max(0.0)) else {
        return source_start + visible.len();
    };
    let line_start = line_start_byte(&visible, cursor.line);
    source_start + clamp_char_boundary(&visible, line_start + cursor.index).min(visible.len())
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

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_buffer::state::BufferState;
    use flavite_syntax::text::{Span, TextByte};
    use iced::advanced::widget::Operation as _;

    fn long_view(lines: usize) -> EditorView {
        let mut text = String::new();
        for index in 0..lines {
            text.push_str(&format!("line {index}\n"));
        }
        EditorView {
            buffer_id: 1,
            revision: 1,
            text: text.into(),
            path: std::path::PathBuf::from("long.md"),
            state: BufferState::Clean,
        }
    }

    #[test]
    fn reconcile_keeps_a_user_scroll_that_leaves_the_caret() {
        let view = long_view(200);
        let mut state = EditorState::default();
        state.reconcile(&view, &HashMap::new());
        assert_eq!(state.last_ensured_caret, 0);

        // The user wheels away from the caret at line 0; the very next
        // reconcile (same caret) must not snap the viewport back.
        state.viewport.first_line = 100;
        state.reconcile(&view, &HashMap::new());
        assert_eq!(state.viewport.first_line, 100);

        // Moving the caret re-anchors around the caret again.
        state.caret = "line 150\n".len() * 150;
        state.reconcile(&view, &HashMap::new());
        assert!(state.viewport.first_line >= 140);
        assert_eq!(state.last_ensured_caret, state.caret);
    }

    #[test]
    fn indexed_viewports_match_source_slices_after_reload_and_empty_lines() {
        let mut state = EditorState::default();
        for (revision, text) in ["", "é\n\n🧑🏽‍💻\n", "no newline", "a\rb\nc\r\n"]
            .into_iter()
            .enumerate()
        {
            let mut view = long_view(0);
            view.revision = revision as u64;
            view.text = text.into();
            state.reconcile(&view, &HashMap::new());
            assert_eq!(
                state.source_line_count(&view.text),
                logical_line_count(text)
            );
            for first_line in [0, 1, 3, 999] {
                for visible_lines in [0, 1, 3] {
                    let viewport = Viewport {
                        first_line,
                        visible_lines,
                        ..Viewport::default()
                    };
                    assert_eq!(
                        state.visible_text(&view.text, viewport),
                        visible_text(text, viewport)
                    );
                }
            }
        }
    }

    #[test]
    fn clearing_link_verdicts_invalidates_previous_resolutions() {
        let mut view = long_view(0);
        view.text = "[[target]]".into();
        let mut state = EditorState::default();
        state.reconcile(
            &view,
            &HashMap::from([("target".into(), LinkResolution::Resolved)]),
        );
        assert_eq!(state.link_spans[0].resolution, LinkResolution::Resolved);
        state.reconcile(&view, &HashMap::new());
        assert_eq!(state.link_spans[0].resolution, LinkResolution::Unresolved);
        state.reconcile(&view, &HashMap::new());
        assert_eq!(state.link_spans[0].resolution, LinkResolution::Unresolved);
    }

    #[test]
    fn scrollbar_geometry_maps_first_line_across_the_track() {
        let (track_y, track_height, thumb_y, thumb_height, max_first) =
            scrollbar_geometry(500.0, 200, 20, 0);
        assert_eq!(max_first, 180);
        assert_eq!(thumb_y, track_y);
        assert!(thumb_height >= 24.0);
        assert!(thumb_height < track_height);
        let (_ty, _th, bottom_y, _bh, _mf) = scrollbar_geometry(500.0, 200, 20, 180);
        assert!(bottom_y > thumb_y);
        // The thumb always stays inside the track.
        assert!(bottom_y + thumb_height <= track_y + track_height + 0.01);
        // Out-of-range first lines clamp.
        let (_, _, _, _, clamped_max) = scrollbar_geometry(500.0, 200, 20, 999);
        assert_eq!(clamped_max, 180);
    }

    fn key_press() -> iced::Event {
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Character("a".into()),
            modified_key: iced::keyboard::Key::Character("a".into()),
            physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::KeyA),
            location: iced::keyboard::Location::Standard,
            modifiers: Modifiers::default(),
            text: Some("a".into()),
            repeat: false,
        })
    }

    #[test]
    fn input_gating_never_blocks_pointer_events() {
        let mouse_press =
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left));
        let mouse_move = iced::Event::Mouse(iced::mouse::Event::CursorMoved {
            position: Point::new(1.0, 1.0),
        });
        // Modal-overlay and find-focus flags suppress keyboard/IME input
        // only; mouse selection must stay live in every combination.
        for input_enabled in [true, false] {
            for keyboard_enabled in [true, false] {
                assert!(
                    !keyboard_input_blocked(input_enabled, keyboard_enabled, &mouse_press),
                    "pointer press blocked by (input={input_enabled}, keyboard={keyboard_enabled})"
                );
                assert!(
                    !keyboard_input_blocked(input_enabled, keyboard_enabled, &mouse_move),
                    "pointer move blocked by (input={input_enabled}, keyboard={keyboard_enabled})"
                );
            }
        }
        // Keyboard input is blocked by either flag.
        assert!(keyboard_input_blocked(false, true, &key_press()));
        assert!(keyboard_input_blocked(true, false, &key_press()));
        assert!(!keyboard_input_blocked(true, true, &key_press()));
    }

    #[test]
    fn cached_shaping_is_reused_until_the_key_changes() {
        let text = "one\ntwo\nthree";
        let viewport = Viewport {
            visible_lines: 2,
            ..Viewport::default()
        };
        let mut state = EditorState::default();
        let buffer_ptr = state.shaped(text, 1, viewport, 300.0).0 as *const ShapingBuffer;
        let (visible, source_start) = {
            let (_, visible, source_start) = state.shaped(text, 1, viewport, 300.0);
            (visible, source_start)
        };
        assert_eq!(source_start, 0);
        assert_eq!(visible, "one\ntwo\n");
        // Same key: the cached buffer is reused (identical allocation).
        let (again, _, _) = state.shaped(text, 1, viewport, 300.0);
        assert!(std::ptr::eq(buffer_ptr, again));
        // A different viewport window reshapes.
        let scrolled = Viewport {
            first_line: 1,
            ..viewport
        };
        let (_, visible3, start3) = state.shaped(text, 1, scrolled, 300.0);
        assert_eq!(visible3, "two\nthree");
        assert_eq!(start3, text.find("two").unwrap());
        // Same length, different revision: still reshapes (revision in key).
        let swapped = "six\nten\nseven";
        assert_eq!(swapped.len(), text.len());
        let (_, visible4, _) = state.shaped(swapped, 2, viewport, 300.0);
        assert_eq!(visible4, "six\nten\n");
    }

    #[test]
    fn ime_preedit_is_transient_and_commit_emits_one_edit() {
        let view = EditorView::from(&test_view("hello", 12));
        let mut state = EditorState {
            caret: 2,
            anchor: 2,
            ..EditorState::default()
        };
        assert_eq!(
            state.input_method_event(&view, &input_method::Event::Opened),
            None
        );
        assert!(!state.is_composing(), "an open IME without preedit is idle");
        assert_eq!(
            state.input_method_event(
                &view,
                &input_method::Event::Preedit("にほん".into(), Some(0..9)),
            ),
            None
        );
        assert!(state.is_composing());
        assert_eq!(state.composition.as_ref().unwrap().preedit, "にほん");
        // winit clears preedit immediately before its commit event. That clear
        // must not end the composition or lose the captured replacement span.
        assert_eq!(
            state.input_method_event(&view, &input_method::Event::Preedit(String::new(), None)),
            None
        );
        let action = state
            .input_method_event(&view, &input_method::Event::Commit("日本語".into()))
            .expect("one committed edit");
        let (_, revision, edits) = action.as_edit().expect("edit action");
        assert_eq!(revision, view.revision);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range, Span::new(TextByte(2), TextByte(2)));
        assert_eq!(edits[0].replacement, "日本語");
        assert_eq!(state.caret(), 2 + "日本語".len());
        assert!(!state.is_composing());
    }

    #[test]
    fn cancelled_ime_composition_never_changes_the_document() {
        let view = EditorView::from(&test_view("hello", 12));
        let mut state = EditorState {
            caret: 3,
            anchor: 3,
            focused: true,
            ime_active: true,
            ..EditorState::default()
        };
        state.input_method_event(&view, &input_method::Event::Opened);
        state.input_method_event(
            &view,
            &input_method::Event::Preedit("pinyin".into(), Some(0..6)),
        );
        assert!(state.is_composing());
        assert_eq!(
            state.input_method_event(&view, &input_method::Event::Closed),
            None
        );
        assert!(!state.is_composing());
        assert!(state.ime_active);
        assert_eq!(state.caret(), 3);

        // Switching the input method back on while the editor retains focus
        // must start a new composition without a second pointer click.
        state.input_method_event(&view, &input_method::Event::Opened);
        state.input_method_event(
            &view,
            &input_method::Event::Preedit("かな".into(), Some(0..2)),
        );
        assert!(state.is_composing());
        let committed = state
            .input_method_event(&view, &input_method::Event::Commit("仮名".into()))
            .expect("reopened IME commits through one guarded edit");
        let (_, _, edits) = committed.as_edit().expect("edit action");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range, Span::new(TextByte(3), TextByte(3)));
        assert_eq!(edits[0].replacement, "仮名");
        assert_eq!(state.caret(), 3 + "仮名".len());
        assert!(!state.is_composing());
    }

    #[test]
    fn host_operation_observes_the_focused_editor_caret_and_composition() {
        let view = EditorView::from(&test_view("text", 12));
        let mut state = EditorState {
            buffer_id: Some(view.buffer_id),
            revision: Some(view.revision),
            caret: 2,
            anchor: 1,
            focused: true,
            composition: Some(Composition {
                start: 2,
                end: 2,
                preedit: "かな".into(),
                selection: None,
            }),
            ..EditorState::default()
        };
        let mut operation = inspect_focused_editor();
        operation.custom(None, Rectangle::with_size(Size::ZERO), &mut state);
        match operation.finish() {
            iced::advanced::widget::operation::Outcome::Some(Some(snapshot)) => {
                assert_eq!(snapshot.buffer_id, view.buffer_id);
                assert_eq!(snapshot.revision, view.revision);
                assert_eq!(snapshot.caret, 2);
                assert_eq!(snapshot.selection, (1, 2));
                assert!(snapshot.focused);
                assert!(snapshot.composing);
            }
            _ => panic!("focused editor snapshot was not returned"),
        }
    }

    #[test]
    fn accessibility_selection_operation_updates_live_editor_and_blocks_ime() {
        let view = EditorView::from(&test_view("text", 12));
        let mut state = EditorState {
            buffer_id: Some(view.buffer_id),
            revision: Some(view.revision),
            caret: 2,
            anchor: 2,
            focused: false,
            ..EditorState::default()
        };
        let mut operation = set_focused_editor_selection(view.buffer_id, view.revision, 1, 4);
        operation.custom(None, Rectangle::with_size(Size::ZERO), &mut state);
        assert_eq!(state.selection(), (1, 4));
        assert!(state.focused);
        assert!(matches!(
            operation.finish(),
            iced::advanced::widget::operation::Outcome::Some(Some(snapshot))
                if snapshot.caret == 4 && snapshot.selection == (1, 4)
        ));

        state.composition = Some(Composition {
            start: 0,
            end: 0,
            preedit: "かな".into(),
            selection: None,
        });
        let mut blocked = set_focused_editor_selection(view.buffer_id, view.revision, 0, 0);
        blocked.custom(None, Rectangle::with_size(Size::ZERO), &mut state);
        assert_eq!(state.selection(), (1, 4));
        assert!(matches!(
            blocked.finish(),
            iced::advanced::widget::operation::Outcome::Some(None)
        ));
    }

    #[test]
    fn accessibility_focus_operation_is_bound_to_buffer_revision() {
        let view = EditorView::from(&test_view("text", 12));
        let mut state = EditorState {
            buffer_id: Some(view.buffer_id),
            revision: Some(view.revision),
            caret: 3,
            anchor: 1,
            focused: false,
            ..EditorState::default()
        };
        let mut stale = focus_editor(view.buffer_id, view.revision + 1);
        stale.custom(None, Rectangle::with_size(Size::ZERO), &mut state);
        assert!(!state.focused);
        assert!(matches!(
            stale.finish(),
            iced::advanced::widget::operation::Outcome::Some(None)
        ));

        let mut current = focus_editor(view.buffer_id, view.revision);
        current.custom(None, Rectangle::with_size(Size::ZERO), &mut state);
        assert!(state.focused);
        assert!(state.ime_active);
        assert_eq!(state.selection(), (1, 3));
        assert!(matches!(
            current.finish(),
            iced::advanced::widget::operation::Outcome::Some(Some(snapshot))
                if snapshot.buffer_id == view.buffer_id && snapshot.revision == view.revision
        ));
    }

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
    fn autocomplete_selection_emits_revision_guarded_link_insert() {
        let view = test_view("[[In", 97);
        let widget = EditorWidget::new(&view, |action| action).with_candidates(Arc::from([
            NoteCandidate::new("notes/inbox", "Inbox"),
            NoteCandidate::new("Ideas", "Ideas"),
        ]));
        let mut state = EditorState {
            caret: view.text.len(),
            anchor: view.text.len(),
            ..EditorState::default()
        };
        state.open_autocomplete(0);
        state.autocomplete_query = "In".into();
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        assert!(widget.execute_key(
            &mut state,
            &Key::Named(Named::Enter),
            Modifiers::empty(),
            None,
            &mut clipboard,
            &mut shell,
        ));
        drop(shell);
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            messages[0],
            EditorAction::Edit {
                expected_revision: 11,
                ..
            }
        ));
        let (_, _, edits) = messages[0].as_edit().expect("edit action");
        assert_eq!(edits[0].range, Span::new(TextByte(0), TextByte(4)));
        assert_eq!(edits[0].replacement, "[[notes/inbox|Inbox]]");
        let invocation = messages[0]
            .clone()
            .into_invocation(88)
            .expect("registry invocation");
        assert_eq!(invocation.command_id, "apply_edits");
        assert_eq!(invocation.args["ops"][0]["range"][0], 0);
        assert_eq!(
            invocation.args["ops"][0]["replacement"],
            "[[notes/inbox|Inbox]]"
        );
        assert_eq!(state.caret(), "[[notes/inbox|Inbox]]".len());
        assert!(state.autocomplete_start.is_none());
    }

    #[test]
    fn autocomplete_create_row_opens_unresolved_note_for_creation() {
        let view = test_view("[[New", 103);
        let widget = EditorWidget::new(&view, |action| action)
            .with_candidates(Arc::from([NoteCandidate::new("Ideas", "Ideas")]));
        let mut state = EditorState {
            caret: view.text.len(),
            anchor: view.text.len(),
            ..EditorState::default()
        };
        state.open_autocomplete(0);
        state.autocomplete_query = "New note".into();
        state.autocomplete_selected = 0; // no matching notes, so the create row is selected
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);

        assert!(widget.execute_key(
            &mut state,
            &Key::Named(Named::Enter),
            Modifiers::empty(),
            None,
            &mut clipboard,
            &mut shell,
        ));
        drop(shell);

        assert_eq!(messages.len(), 1);
        let navigation = messages[0].navigation().expect("create navigation");
        assert_eq!(navigation.target, "New note");
        assert!(navigation.create_if_unresolved);
        assert_eq!(navigation.span, Span::new(TextByte(0), TextByte(5)));
        assert!(state.autocomplete_start.is_none());
    }

    #[test]
    fn link_insert_supports_readable_aliases_and_opt_in_stable_form() {
        let mut candidate = NoteCandidate::new("notes/concepts", "Concepts");
        candidate.stable_id = Some("01J9ZK".into());
        let format = |candidate: &NoteCandidate, stable: bool| {
            flavite_syntax::link_format::format_candidate_link(
                &candidate.target,
                &candidate.title,
                candidate.stable_id.as_deref(),
                stable,
            )
        };
        assert_eq!(format(&candidate, false), "[[notes/concepts|Concepts]]");
        assert_eq!(format(&candidate, true), "[[id:01J9ZK|Concepts]]");
    }

    #[test]
    fn link_activation_emits_target_syntax_and_never_opens_embeds_as_notes() {
        let text = "[[Ideas#Plan|Ideas]] ![[diagram.png]]";
        let spans = crate::links::scan_links(text);
        let view = EditorView {
            buffer_id: 88,
            revision: 15,
            text: text.into(),
            path: std::path::PathBuf::from("source.md"),
            state: flavite_buffer::state::BufferState::Clean,
        };
        let action = link_action_at(&view, &spans, 4).expect("link activation");
        assert!(matches!(
            action,
            EditorAction::Navigate {
                buffer_id: 88,
                expected_revision: 15,
                ..
            }
        ));
        let navigation = action.navigation().expect("navigation action");
        assert_eq!(navigation.target, "Ideas");
        assert_eq!(navigation.alias.as_deref(), Some("Ideas"));
        assert_eq!(navigation.anchor.as_deref(), Some("Plan"));
        assert!(navigation.create_if_unresolved);
        assert!(link_action_at(&view, &spans, text.find("diagram").unwrap()).is_none());
    }

    #[test]
    fn cosmic_text_hit_test_returns_utf8_boundary() {
        let text = "a🧑🏽‍💻e\u{301}";
        let viewport = Viewport {
            visible_lines: 1,
            ..Viewport::default()
        };
        let byte = hit_test_visible(text, viewport, 45.0, 4.0, 200.0);
        assert!(text.is_char_boundary(byte));
    }

    #[test]
    fn cosmic_text_layout_contains_the_editor_text() {
        let text = "Heading\n\nbody\n\n- item\n- final visual editor input";
        let shaped = shape_text(text, 400.0, TypeMetrics::default());
        let runs = shaped.layout_runs().collect::<Vec<_>>();
        assert!(!runs.is_empty());
        assert!(runs.iter().any(|run| run.text == "Heading"));
        let (_, y) = caret_position(&runs, text, text.len());
        assert_eq!(y, runs.last().expect("non-empty layout").line_top);
    }

    #[test]
    fn cosmic_text_layout_wraps_long_lines() {
        let shaped = shape_text("abcdefghij", 24.0, TypeMetrics::default());
        assert!(shaped.layout_runs().count() > 1);
    }

    #[test]
    fn caret_moves_by_grapheme_clusters() {
        let text = "a🧑🏽‍💻e\u{301}";
        let end = text.len();
        assert_eq!(previous_grapheme_boundary(text, end), "a🧑🏽‍💻".len());
        assert_eq!(next_grapheme_boundary(text, 1), "a🧑🏽‍💻".len());
    }

    #[test]
    fn keyed_views_restore_lower_sequences_without_inheriting_shared_buffer_carets() {
        let view = EditorView::from(&test_view("abcdef", 77));
        let snapshot = |sequence, caret| EditorInteractionSnapshot {
            sequence,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret,
            selection: (caret, caret),
            viewport_anchor: 0,
            focused: true,
            composing: false,
        };
        let first: iced::Element<'_, EditorAction> =
            EditorWidget::from_view(view.clone(), |action| action)
                .with_view_key(1, 1)
                .with_initial_interaction(snapshot(10, 1))
                .into();
        let mut tree = Tree::new(&first);
        first.as_widget().diff(&mut tree);
        assert_eq!(tree.state.downcast_ref::<EditorState>().caret(), 1);
        let second: iced::Element<'_, EditorAction> =
            EditorWidget::from_view(view.clone(), |action| action)
                .with_view_key(2, 1)
                .with_initial_interaction(snapshot(1, 5))
                .into();
        second.as_widget().diff(&mut tree);
        assert_eq!(tree.state.downcast_ref::<EditorState>().caret(), 5);
        let reopened: iced::Element<'_, EditorAction> =
            EditorWidget::from_view(view.clone(), |action| action)
                .with_view_key(2, 2)
                .with_initial_interaction(snapshot(1, 3))
                .into();
        reopened.as_widget().diff(&mut tree);
        assert_eq!(tree.state.downcast_ref::<EditorState>().caret(), 3);
    }

    fn test_view(text: &str, buffer_id: u64) -> BufferView {
        BufferView {
            buffer_id,
            revision: 11,
            text: text.to_owned(),
            base_hash: Vec::new(),
            state: flavite_buffer::state::BufferState::Clean,
            path: std::path::PathBuf::from("note.md"),
            source_format: flavite_api::SourceFormat::utf8(),
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
        assert!(matches!(
            messages[0],
            EditorAction::Edit {
                buffer_id: 91,
                expected_revision: 11,
                ..
            }
        ));
        let (_, _, edits) = messages[0].as_edit().expect("edit action");
        assert_eq!(edits[0].range, Span::new(TextByte(2), TextByte(2)));
        assert_eq!(edits[0].replacement, "é");
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
            messages[0].as_edit().unwrap().2[0].range,
            Span::new(TextByte(1), TextByte(1))
        );
        assert_eq!(
            messages[1].as_edit().unwrap().2[0].range,
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
        assert!(matches!(
            messages[0],
            EditorAction::History {
                buffer_id: 18,
                direction: crate::surface::HistoryDirection::Undo,
                ..
            }
        ));
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
        let shaped = shape_text(text, 300.0, TypeMetrics::default());
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

        state.place_caret_from_point(text, 0, target.x + 0.1, 1.0, 300.0, false);
        assert_eq!(state.caret(), expected_byte);
        assert_eq!(state.selection(), (expected_byte, expected_byte));
        state.place_caret_from_point(text, 0, first_run.glyphs[1].x + 0.1, 1.0, 300.0, true);
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
            state: flavite_buffer::state::BufferState::Dirty,
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
        state.reconcile(&shorter_same_buffer, &HashMap::new());
        assert_eq!(state.selection(), (1, 1));

        let reopened = EditorView {
            buffer_id: 2,
            text: "new".into(),
            ..shorter_same_buffer
        };
        state.focused = true;
        state.reconcile(&reopened, &HashMap::new());
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
