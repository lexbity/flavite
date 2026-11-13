//! Custom iced editor widget over a revisioned, immutable API view.

use flavite_api::CompletionCandidate;

use crate::keymap::{KeyAction, key_action};
use crate::links::{LinkResolution, LinkSpan, match_candidates, resolve_link, scan_note};
use crate::navigation::VerticalMotion;
use crate::selections::EditorSelections;
use crate::surface::{EditorAction, EditorSurface, EditorView, LinkNavigation};
use crate::viewport::{Viewport, clamp_char_boundary};
use cosmic_text::{Attrs, Buffer as ShapingBuffer, FontSystem, Metrics, Shaping};
#[cfg(feature = "test-support")]
use flavite_api::BufferView;
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

/// The host's own wrap-width computation for layout-dependent commands
/// (`editor.add_caret_above/below` row targets, X05): identical to the
/// widget's wrap width so both sides agree on visual rows.
#[must_use]
pub fn wrap_width_for(bounds_width: f32, max_line_width: Option<f32>) -> f32 {
    let available = (bounds_width - PAD_X * 2.0).max(1.0);
    match max_line_width {
        Some(max) => available.min(max.max(1.0)),
        None => available,
    }
}
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
    let thumb_height = (track_height * shown as f32 / total_lines.max(1) as f32)
        .max(24.0)
        .min(track_height);
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
    /// Editor surface fill (xpi themes: the package's field color; the
    /// built-in palettes keep the shell background).
    pub background: Color,
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
            background: Color::from_rgb(0.098, 0.114, 0.137),
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
type ContextToggle<'a> = Box<dyn Fn(&keyboard::Event) -> bool + 'a>;

pub struct EditorWidget<'a, Message> {
    context_mode: flavite_edit_context::ContextMode,
    context_preview: Option<flavite_edit_context::ContextCommand>,
    on_context: Option<Box<dyn Fn(crate::context::ContextRequest) -> Message + 'a>>,
    context_toggle: Option<ContextToggle<'a>>,
    view: EditorView,
    candidates: Arc<[CompletionCandidate]>,
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
    /// Create an editor from the API's immutable revisioned view. Production
    /// shells share an already-owned revision through [`Self::from_view`];
    /// this copy-making constructor exists for deterministic tests.
    #[cfg(feature = "test-support")]
    pub fn new(view: &BufferView, on_action: impl Fn(EditorAction) -> Message + 'a) -> Self {
        Self::from_view(EditorView::from(view), on_action)
    }

    /// Create an editor from an already shared, API-owned text revision.
    pub fn from_view(view: EditorView, on_action: impl Fn(EditorAction) -> Message + 'a) -> Self {
        Self {
            context_mode: flavite_edit_context::ContextMode::Off,
            context_preview: None,
            on_context: None,
            context_toggle: None,
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

    /// Interpret command-state physical keys without inserting source text.
    #[must_use]
    pub fn with_context_mode(
        mut self,
        mode: flavite_edit_context::ContextMode,
        on_context: impl Fn(crate::context::ContextRequest) -> Message + 'a,
    ) -> Self {
        self.context_mode = mode;
        self.on_context = Some(Box::new(on_context));
        self
    }

    /// Paint a prospective implicit range without changing editor state.
    #[must_use]
    pub fn with_command_preview(
        mut self,
        command: Option<flavite_edit_context::ContextCommand>,
    ) -> Self {
        self.context_preview = command;
        self
    }

    fn emit_context(
        &self,
        state: &mut EditorState,
        event: crate::context::ContextEvent,
        shell: &mut Shell<'_, Message>,
    ) {
        if let Some(callback) = &self.on_context {
            state.interaction_sequence = state.interaction_sequence.saturating_add(1);
            shell.publish(callback(crate::context::ContextRequest {
                buffer: self.view.buffer_id,
                revision: self.view.revision,
                selection: (state.selections.anchor(), state.selections.caret()),
                selections: state.selections.pairs(),
                sequence: state.interaction_sequence,
                event,
            }));
            shell.capture_event();
            shell.request_redraw();
        }
    }

    fn emit_context_commit(&self, state: &EditorState, text: &str, shell: &mut Shell<'_, Message>) {
        if let Some(callback) = &self.on_context {
            shell.publish(callback(crate::context::ContextRequest {
                buffer: self.view.buffer_id,
                revision: self.view.revision,
                selection: (state.selections.anchor(), state.selections.caret()),
                selections: state.selections.pairs(),
                sequence: state.interaction_sequence,
                event: crate::context::ContextEvent::Commit(crate::input::CapturedInput {
                    buffer: self.view.buffer_id,
                    observed_revision: self.view.revision,
                    sequence: state.interaction_sequence,
                    action: KeyAction::Insert,
                    text: text.into(),
                    extend: false,
                    document_boundary: false,
                    vertical: None,
                    preferred_x: Vec::new(),
                }),
            }));
            shell.capture_event();
            shell.request_redraw();
        }
    }

    /// Let the host's resolved keybinding catalogue own the toggle chord.
    #[must_use]
    pub fn with_context_toggle(mut self, matches: impl Fn(&keyboard::Event) -> bool + 'a) -> Self {
        self.context_toggle = Some(Box::new(matches));
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
        wrap_width_for(bounds_width, self.max_line_width)
    }

    /// Supply an indexed title/alias snapshot for in-memory autocomplete.
    #[must_use]
    pub fn with_candidates(mut self, candidates: Arc<[CompletionCandidate]>) -> Self {
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
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn state_for_test() -> EditorState {
        EditorState::default()
    }

    fn emit(&self, shell: &mut Shell<'_, Message>, action: EditorAction) {
        shell.publish((self.on_action)(action));
    }

    fn synchronize(&self, state: &mut EditorState) {
        if state.view_key != self.view_key
            || state.buffer_id.is_some_and(|id| id != self.view.buffer_id)
        {
            *state = EditorState::default();
            state.view_key = self.view_key;
        }
        if state.type_metrics != self.type_metrics {
            state.type_metrics = self.type_metrics;
            state.viewport.line_height = self.type_metrics.line_height;
            state.shaping = None;
            state.selections.clear_preferred_x();
        }
        // Resolver verdicts are immutable snapshots. Reuse them on focus and
        // selection redraws; a new source revision always reapplies verdicts.
        let same_resolutions = state
            .resolution_snapshot
            .as_ref()
            .is_some_and(|previous| Arc::ptr_eq(previous, &self.link_resolutions));
        let source_changed = state.revision != Some(self.view.revision);
        let refresh = !same_resolutions || source_changed;
        state.reconcile_view(&self.view, &self.link_resolutions, refresh);
        state.resolution_snapshot = Some(self.link_resolutions.clone());
        if self.context_mode == flavite_edit_context::ContextMode::Command {
            state.close_autocomplete();
            state.composition = None;
        }
        if let Some(interaction) = self.initial_interaction.clone() {
            if source_changed && !state.controlled_pending && interaction.sequence > 0 {
                // Pointer release/blur events can have a larger local sequence
                // while still naming the old text. A new acknowledged source
                // revision owns its selection and focus. Keep the native
                // capture counter monotonic after installing that receipt.
                let captured_sequence = state.interaction_sequence;
                state.interaction_sequence = interaction.sequence - 1;
                state.restore_interaction(&self.view.text, interaction);
                state.interaction_sequence = state.interaction_sequence.max(captured_sequence);
            } else if self.on_input_intent.is_some()
                && state.controlled_pending
                && interaction.sequence > state.controlled_ack
            {
                let captured_sequence = state.interaction_sequence;
                let incoming_sequence = interaction.sequence;
                state.interaction_sequence = incoming_sequence.saturating_sub(1);
                state.restore_interaction(&self.view.text, interaction);
                state.interaction_sequence = state.interaction_sequence.max(captured_sequence);
                state.controlled_ack = incoming_sequence;
                state.controlled_pending = incoming_sequence < captured_sequence;
                if self
                    .view
                    .text
                    .get(..state.selections.caret())
                    .is_some_and(|text| text.ends_with("[["))
                {
                    state.open_autocomplete(state.selections.caret() - 2);
                }
            } else {
                state.restore_interaction(&self.view.text, interaction);
            }
        }
        state.refresh_completion(&self.view.text);
    }

    /// The widget-owned autocomplete interception (the single captured-intent
    /// contract leaves no other keyboard interpretation here: every other key
    /// publishes a [`crate::input::CapturedInput`] for the host to interpret).
    /// Returns whether the key was consumed by the popup.
    fn autocomplete_key(
        &self,
        state: &mut EditorState,
        key: &Key,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        if state.autocomplete_start.is_none() {
            return false;
        }
        match key {
            Key::Named(Named::Escape) => {
                state.close_autocomplete();
                true
            }
            Key::Named(Named::ArrowUp) => {
                state.autocomplete_selected = state.autocomplete_selected.saturating_sub(1);
                true
            }
            Key::Named(Named::ArrowDown) => {
                let count = autocomplete_count(self, state);
                state.autocomplete_selected = state
                    .autocomplete_selected
                    .saturating_add(1)
                    .min(count.saturating_sub(1));
                true
            }
            Key::Named(Named::Enter) => {
                let matches = state.candidate_matches(&self.candidates);
                if let Some(candidate) = matches.get(state.autocomplete_selected) {
                    let start = state.autocomplete_start.unwrap_or(state.selections.caret());
                    self.emit(
                        shell,
                        EditorAction::Complete {
                            buffer_id: self.view.buffer_id,
                            expected_revision: self.view.revision,
                            start,
                            end: state.selections.caret(),
                            prefix: self.view.text[start..state.selections.caret()].to_owned(),
                            candidate: (*candidate).clone(),
                        },
                    );
                } else if state.autocomplete_selected == matches.len()
                    && !state.autocomplete_query.trim().is_empty()
                {
                    let start = state.autocomplete_start.unwrap_or(state.selections.caret());
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
                                    flavite_syntax::TextByte(state.selections.caret()),
                                ),
                                create_if_unresolved: true,
                            },
                        ),
                    );
                }
                state.close_autocomplete();
                state.ensure_caret_visible(&self.view.text);
                true
            }
            _ => false,
        }
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
    resolution_snapshot: Option<Arc<HashMap<String, LinkResolution>>>,
    selections: EditorSelections,
    focused: bool,
    ime_active: bool,
    selecting: bool,
    modifiers: Modifiers,
    autocomplete_start: Option<usize>,
    autocomplete_query: String,
    autocomplete_selected: usize,
    autocomplete_matches: std::cell::RefCell<Option<CompletionCache>>,
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
    /// Sticky visual column for consecutive vertical movements.
    text_width: f32,
    last_click: Option<mouse::Click>,
    selection_unit: SelectionUnit,
    selection_origin: (usize, usize),
    /// The press byte of a Mod-added member while a Mod drag extends it
    /// (E-14); transient geometry, never a second committed representation.
    mod_drag: Option<usize>,
}

#[derive(Debug, Default, Clone, Copy)]
enum SelectionUnit {
    #[default]
    Character,
    Word,
    Line,
}

#[derive(Debug)]
struct CompletionCache {
    candidates: Arc<[CompletionCandidate]>,
    query: String,
    matches: Arc<[CompletionCandidate]>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorInteractionSnapshot {
    pub sequence: u64,
    pub buffer_id: u64,
    pub revision: u64,
    pub caret: usize,
    pub selection: (usize, usize),
    /// Every live member as `[id, anchor, head]` triples (canonical order;
    /// FEUX-0002 X04: the primary pair above stays for single-selection
    /// consumers).
    pub selections: Vec<[usize; 3]>,
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
                caret: state.selections.caret(),
                selection: state.selection(),
                selections: state.selections.set().to_triples(),
                viewport_anchor: 0,
                focused: true,
                composing: state.is_composing(),
            });
        }

        fn finish(
            &self,
        ) -> iced::advanced::widget::operation::Outcome<Option<EditorInteractionSnapshot>> {
            iced::advanced::widget::operation::Outcome::Some(self.0.clone())
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
            // Offsets were validated by the host against the current text;
            // the caret may sit past the local view until its refresh.
            state
                .selections
                .replace_beyond_text(&[(self.anchor, self.focus)], 0);
            state.focused = true;
            state.ime_active = true;
            self.result = state.interaction_snapshot(None);
        }

        fn finish(
            &self,
        ) -> iced::advanced::widget::operation::Outcome<Option<EditorInteractionSnapshot>> {
            iced::advanced::widget::operation::Outcome::Some(self.result.clone())
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
    expected_bounds: Option<Rectangle>,
) -> impl iced::advanced::widget::Operation<Option<EditorInteractionSnapshot>> {
    struct FocusEditor {
        expected_buffer_id: u64,
        expected_revision: u64,
        expected_bounds: Option<Rectangle>,
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
            bounds: Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            let Some(state) = state.downcast_mut::<EditorState>() else {
                return;
            };
            if self
                .expected_bounds
                .is_some_and(|expected| !bounds.contains(expected.center()))
                || state.is_composing()
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
            iced::advanced::widget::operation::Outcome::Some(self.result.clone())
        }
    }

    FocusEditor {
        expected_buffer_id,
        expected_revision,
        expected_bounds,
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
    fn selection_bounds(text: &str, byte: usize, unit: SelectionUnit) -> (usize, usize) {
        use unicode_segmentation::UnicodeSegmentation;
        match unit {
            SelectionUnit::Character => (byte, byte),
            SelectionUnit::Word => {
                // A drag near the end of a large note must not segment all
                // preceding paragraphs on every pointer event.
                let (line_start, line_end) = line_bounds(text, byte);
                if byte == line_end && byte < text.len() {
                    return (byte, byte + 1);
                }
                text[line_start..line_end]
                    .split_word_bound_indices()
                    .find(|(start, word)| {
                        line_start + start <= byte && byte < line_start + start + word.len()
                    })
                    .map_or((byte, byte), |(start, word)| {
                        (line_start + start, line_start + start + word.len())
                    })
            }
            SelectionUnit::Line => {
                let (start, end) = line_bounds(text, byte);
                (start, if end < text.len() { end + 1 } else { end })
            }
        }
    }

    /// The pointer press vocabulary (FEUX-0002 E-14): a plain click replaces
    /// the set with the unit selection; a Mod click toggles a caret at the
    /// position (removal absorbs boundary carets; the last member is never
    /// removed, and a Mod drag then extends the added member into a range);
    /// Shift extends the primary member's head; double/triple select the
    /// word/line (as an added member under Mod).
    fn select_click(
        &mut self,
        text: &str,
        byte: usize,
        kind: mouse::click::Kind,
        extend: bool,
        toggle: bool,
    ) {
        self.selections.clear_preferred_x();
        self.close_autocomplete();
        self.selection_unit = match kind {
            mouse::click::Kind::Single => SelectionUnit::Character,
            mouse::click::Kind::Double => SelectionUnit::Word,
            mouse::click::Kind::Triple => SelectionUnit::Line,
        };
        let (start, end) = Self::selection_bounds(text, byte, self.selection_unit);
        if toggle {
            self.selection_unit = SelectionUnit::Character;
            let added = self.selections.toggle_member(text, byte, (start, end));
            // A removal must not start a drag-add; an addition extends.
            self.mod_drag = added.then_some(byte);
            self.selection_origin = (byte, byte);
            return;
        }
        if extend {
            self.move_to(text, byte, true);
            self.selection_unit = SelectionUnit::Character;
            self.selection_origin = (self.selections.anchor(), self.selections.anchor());
            return;
        }
        self.selections.replace_with(text, &[(start, end)], 0);
        self.selection_origin = (start, end);
    }

    fn extend_pointer_selection(&mut self, text: &str, byte: usize) {
        let (start, end) = Self::selection_bounds(text, byte, self.selection_unit);
        if let Some(origin) = self.mod_drag {
            // Mod-drag extends the member added at the press into a range.
            let member = if byte < origin { (end, origin) } else { (origin, end) };
            self.selections.set_member_at_point(text, origin, member);
            return;
        }
        let (origin_start, origin_end) = self.selection_origin;
        if byte < origin_start {
            self.selections
                .replace_with(text, &[(origin_end, start)], 0);
        } else {
            self.selections
                .replace_with(text, &[(origin_start, end)], 0);
        }
    }

    /// Move the primary member's head (the delivered extend rule), keeping
    /// every other member of the set.
    fn move_to(&mut self, text: &str, byte: usize, extend: bool) {
        let anchor = if extend {
            self.selections.anchor()
        } else {
            byte
        };
        if self.selections.len() == 1 {
            self.selections.replace_with(text, &[(anchor, byte)], 0);
        } else {
            self.selections.move_primary_head(text, anchor, byte);
        }
    }

    fn vertical_motion(&self, action: KeyAction, width: f32) -> VerticalMotion {
        let page = self.viewport.visible_lines.saturating_sub(2).max(1) as i32;
        let rows = match action {
            KeyAction::Up => -1,
            KeyAction::Down => 1,
            KeyAction::PageUp => -page,
            KeyAction::PageDown => page,
            _ => unreachable!("vertical action"),
        };
        VerticalMotion {
            rows,
            width,
            metrics: self.type_metrics,
        }
    }

    fn candidate_matches(
        &self,
        candidates: &Arc<[CompletionCandidate]>,
    ) -> Arc<[CompletionCandidate]> {
        let mut cache = self.autocomplete_matches.borrow_mut();
        if let Some(cached) = cache.as_ref()
            && cached.query == self.autocomplete_query
            && Arc::ptr_eq(&cached.candidates, candidates)
        {
            return cached.matches.clone();
        }
        let matches: Arc<[CompletionCandidate]> =
            match_candidates(candidates, &self.autocomplete_query, 8)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
                .into();
        *cache = Some(CompletionCache {
            candidates: candidates.clone(),
            query: self.autocomplete_query.clone(),
            matches: matches.clone(),
        });
        matches
    }

    /// Caret byte offset (the primary member's head).
    #[must_use]
    pub fn caret(&self) -> usize {
        self.selections.caret()
    }

    /// Current primary selection in normalized TextByte coordinates.
    #[must_use]
    pub fn selection(&self) -> (usize, usize) {
        let member = self.selections.primary();
        (member.start(), member.end())
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
            caret: self.selections.caret(),
            selection: self.selection(),
            selections: self.selections.set().to_triples(),
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
        // Restore the whole acknowledged set (X04); the primary pair stays
        // authoritative for consumers predating multi-member support.
        let members: Vec<(usize, usize)> = if interaction.selections.is_empty() {
            vec![(
                clamp_char_boundary(text, interaction.selection.0),
                clamp_char_boundary(text, interaction.selection.1),
            )]
        } else {
            interaction
                .selections
                .iter()
                .map(|[_, anchor, head]| {
                    (
                        clamp_char_boundary(text, *anchor),
                        clamp_char_boundary(text, *head),
                    )
                })
                .collect()
        };
        self.selections.replace_with(text, &members, 0);
        self.viewport.first_line = text[..interaction.viewport_anchor.min(text.len())]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        self.focused = interaction.focused;
        self.ime_active = interaction.focused;
        self.interaction_sequence = interaction.sequence;
        // A reparented editor has not measured its viewport yet. Preserve
        // the host's scroll anchor instead of treating the default one-row
        // height as a reason to scroll the caret to the top.
        if self.focused && self.text_width > 0.0 {
            self.ensure_caret_visible(text);
        }
        self.last_ensured_caret = self.selections.caret();
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
                let after = start + committed.len();
                self.selections.replace_beyond_text(&[(after, after)], 0);
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

    fn refresh_completion(&mut self, text: &str) {
        let Some(start) = self.autocomplete_start else {
            return;
        };
        let query = text
            .get(start..self.selections.caret())
            .and_then(|prefix| prefix.strip_prefix("[["));
        if self.selections.anchor() == self.selections.caret()
            && let Some(query) = query
            && !query.contains(['[', ']', '\n', '\r', '|', '#'])
        {
            self.autocomplete_query = query.into();
        } else {
            self.close_autocomplete();
        }
    }

    /// Reconcile widget state with a replacement controlled buffer view
    /// (reload, journal patch, reopen under the same tab role).
    pub fn reconcile(
        &mut self,
        view: &EditorView,
        link_resolutions: &HashMap<String, LinkResolution>,
    ) {
        self.reconcile_view(view, link_resolutions, true);
    }

    fn reconcile_view(
        &mut self,
        view: &EditorView,
        link_resolutions: &HashMap<String, LinkResolution>,
        refresh_resolutions: bool,
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
        if refresh_resolutions && (!link_resolutions.is_empty() || self.resolutions_present) {
            for span in &mut self.link_spans {
                let key = flavite_syntax::text::fold_key(&span.payload.target_raw);
                span.resolution = resolve_link(span, link_resolutions.get(&key).copied());
            }
        }
        self.resolutions_present = !link_resolutions.is_empty();
        self.selections.clamp(&view.text);
        self.viewport.line_height = self.type_metrics.line_height;
        self.viewport.first_line = self
            .viewport
            .first_line
            .min(self.source_line_count(&view.text).saturating_sub(1));
        // Only re-anchor the viewport when the caret moved. Running this
        // every frame would snap the view back to the caret and cancel any
        // wheel or scrollbar scroll away from it.
        if self.selections.caret() != self.last_ensured_caret {
            self.ensure_caret_visible(&view.text);
            self.last_ensured_caret = self.selections.caret();
        }
    }

    fn ensure_caret_visible(&mut self, text: &str) {
        let caret = self.selections.caret().min(text.len());
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
        self.selections.clear_preferred_x();
        let byte = self.hit_test_cached(text, revision, self.viewport, x, y, width);
        self.move_to(text, byte, extend);
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
                + crate::viewport::clamp_grapheme_boundary(visible, line_start + cursor.index)
                    .min(visible.len())
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
        state.text_width = self.text_wrap_width(size.width);
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
            Background::Color(self.colors.background),
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
        // Viewport-window painting of every member (FEUX-0002 X04, FR-13):
        // members whose range lies outside the visible source window cost
        // nothing, so paint scales with the viewport, not with N.
        let visible_end = source_start + visible_text.len();
        let visible_members: Vec<_> = state
            .selections
            .members()
            .into_iter()
            .filter(|member| member.start() < visible_end && member.end() > source_start)
            .collect();
        if !visible_members.is_empty() {
            for run in &runs {
                let line_start = source_start + line_start_byte(visible_text, run.line_i);
                for glyph in run.glyphs {
                    let glyph_start = line_start + glyph.start;
                    let glyph_end = line_start + glyph.end;
                    let hit = visible_members
                        .iter()
                        .any(|member| glyph_end > member.start() && glyph_start < member.end());
                    if hit {
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
        if self.context_mode.active()
            && state.composition.is_none()
            && let Some(command) = self.context_preview
        {
            // The implicit-effect outline generalizes to every member before
            // a destructive invocation (FEUX-0002 X04, E-6).
            for member in state.selections.members() {
                let Some(effect) = flavite_edit_context::effect::plan(
                    &self.view.text,
                    (member.anchor(), member.head()),
                    command,
                )
                .filter(|effect| effect.implicit && !effect.range.is_empty())
                else {
                    continue;
                };
                for rect in command_preview_rects(
                    visible_text,
                    source_start,
                    &runs,
                    effect.range,
                    self.type_metrics,
                ) {
                    let rect = Rectangle::new(
                        Point::new(bounds.x + PAD_X + rect.x, bounds.y + PAD_Y + rect.y),
                        rect.size(),
                    );
                    if let Some(rect) = rect.intersection(&clip) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: rect,
                                border: Border {
                                    color: self.colors.text,
                                    width: 1.5,
                                    ..Border::default()
                                },
                                ..renderer::Quad::default()
                            },
                            Background::Color(Color::TRANSPARENT),
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
            // Scanner spans are source-ordered and non-overlapping. Jump
            // directly to this paragraph instead of scanning every link in
            // the note and allocating two vectors for each painted row.
            let first = state
                .link_spans
                .partition_point(|span| span.span.end <= line_start);
            let last = state
                .link_spans
                .partition_point(|span| span.span.start < line_end);
            let active_links = &state.link_spans[first..last];
            let first = state
                .decorations
                .partition_point(|span| span.span.end.0 <= line_start);
            let last = state
                .decorations
                .partition_point(|span| span.span.start.0 < line_end);
            let active_decorations = &state.decorations[first..last];
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
                bounds: Size::new(text_width, (bounds.height - PAD_Y * 2.0).max(1.0)),
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
            let matches = state.candidate_matches(&self.candidates);
            let has_create = !state.autocomplete_query.trim().is_empty();
            if !matches.is_empty() || has_create {
                let row_count = matches.len() + usize::from(has_create);
                let line = self.view.text[..state.selections.caret().min(self.view.text.len())]
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
        if state.focused {
            // Every member's head paints a caret; the primary keeps the
            // delivered Command 4 px rule (X04: primary thick, others
            // standard). Only heads inside the visible window paint.
            let primary = state.selections.primary_id();
            for member in state.selections.members() {
                if member.head() < source_start || member.head() > source_start + visible_text.len()
                {
                    continue;
                }
                let local_caret = member.head().saturating_sub(source_start);
                let (x, y) = caret_position(&runs, visible_text, local_caret);
                let width = if member.id() == primary
                    && self.context_mode == flavite_edit_context::ContextMode::Command
                {
                    4.0
                } else {
                    1.5
                };
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle::new(
                            Point::new(bounds.x + PAD_X + x, bounds.y + PAD_Y + y),
                            Size::new(width, self.type_metrics.line_height),
                        ),
                        border: Border::default(),
                        ..renderer::Quad::default()
                    },
                    Background::Color(self.colors.autocomplete_accent),
                );
            }
        }
    }

    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<EditorState>()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        let mut state = EditorState::default();
        self.synchronize(&mut state);
        iced::advanced::widget::tree::State::new(state)
    }

    fn diff(&self, tree: &mut Tree) {
        self.synchronize(tree.state.downcast_mut::<EditorState>());
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
        _clipboard: &mut dyn Clipboard,
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
        if let iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event
            && self.input_enabled
            && self.keyboard_enabled
            && state.composition.is_none()
            && self.on_context.is_some()
            && let Some(position) = cursor.position_in(layout.bounds())
        {
            let byte = state.hit_test_cached(
                &self.view.text,
                self.view.revision,
                state.viewport,
                position.x - PAD_X,
                position.y - PAD_Y,
                self.text_wrap_width(layout.bounds().width),
            );
            // D-9 against the whole set (E-14): a press inside any member
            // keeps the set; anything else resolves the word under the
            // pointer (or a plain caret).
            let inside = state
                .selections
                .members()
                .iter()
                .any(|member| member.start() <= byte && byte <= member.end());
            let selection = if inside {
                state.selections.pairs()
            } else {
                let (a, b) = flavite_edit_context::word_bounds(&self.view.text, byte);
                vec![(a, b)]
            };
            self.emit_context(
                state,
                crate::context::ContextEvent::EnterAt(selection),
                shell,
            );
            return;
        }
        if let iced::Event::Keyboard(keyboard::Event::KeyPressed { key, repeat, .. }) = event
            && state.focused
            && state.composition.is_none()
            && self.on_context.is_some()
        {
            use crate::context::ContextEvent;
            use flavite_edit_context::ContextCommand;
            if let iced::Event::Keyboard(key_event) = event
                && self
                    .context_toggle
                    .as_ref()
                    .is_some_and(|matches| matches(key_event))
            {
                if !repeat {
                    self.emit_context(state, ContextEvent::Command(ContextCommand::Toggle), shell);
                }
                shell.capture_event();
                return;
            }
            if matches!(key, Key::Named(Named::Escape))
                && state.autocomplete_start.is_none()
                && self.context_mode.active()
            {
                self.emit_context(state, ContextEvent::Command(ContextCommand::Escape), shell);
                return;
            }
        }

        if let iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key,
            physical_key,
            modifiers,
            text,
            repeat,
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
            let context_printable = self.context_mode.active()
                && !modifiers.control()
                && !modifiers.alt()
                && !modifiers.logo()
                && matches!(key, Key::Character(_));
            if (action != KeyAction::Ignore || context_printable) && !completion_control {
                if !matches!(
                    action,
                    KeyAction::Insert | KeyAction::Backspace | KeyAction::Delete | KeyAction::Paste
                ) {
                    state.close_autocomplete();
                }
                let physical = if let keyboard::key::Physical::Code(code) = physical_key {
                    Some(*code)
                } else {
                    None
                };
                let geometry_action = physical
                    .and_then(crate::context::native_code)
                    .and_then(|code| {
                        flavite_edit_context::motion_key(
                            code,
                            crate::context::native_modifiers(*modifiers),
                        )
                    })
                    .map(crate::context::motion_action)
                    .filter(|a| matches!(a, KeyAction::Up | KeyAction::Down))
                    .unwrap_or(action);
                let vertical = if matches!(
                    geometry_action,
                    KeyAction::Up | KeyAction::Down | KeyAction::PageUp | KeyAction::PageDown
                ) {
                    Some(state.vertical_motion(
                        geometry_action,
                        self.text_wrap_width(layout.bounds().width),
                    ))
                } else {
                    state.selections.clear_preferred_x();
                    None
                };
                // The produced text only; clipboard reads happen at the host
                // clipboard seam (FEUX-0002 X04, T-10).
                let committed = text.clone().unwrap_or_default().to_string();
                state.interaction_sequence = state.interaction_sequence.saturating_add(1);
                state.controlled_pending = true;
                let input = crate::input::CapturedInput {
                    buffer: self.view.buffer_id,
                    observed_revision: self.view.revision,
                    sequence: state.interaction_sequence,
                    action,
                    text: committed,
                    extend: modifiers.shift(),
                    document_boundary: modifiers.command() || modifiers.control(),
                    vertical,
                    preferred_x: state.selections.preferred_x_all(),
                };
                if let Some(context) = &self.on_context {
                    shell.publish(context(crate::context::ContextRequest {
                        buffer: self.view.buffer_id,
                        revision: self.view.revision,
                        selection: (state.selections.anchor(), state.selections.caret()),
                        selections: state.selections.pairs(),
                        sequence: state.interaction_sequence,
                        event: crate::context::ContextEvent::Key(crate::context::ContextKey {
                            code: physical.and_then(crate::context::native_code),
                            modifiers: crate::context::native_modifiers(*modifiers),
                            repeat: *repeat,
                            input,
                        }),
                    }));
                } else {
                    shell.publish(callback(input));
                }
                shell.capture_event();
                shell.request_redraw();
                return;
            }
        }
        match event {
            iced::Event::InputMethod(input_event) if state.focused => {
                if self.context_mode == flavite_edit_context::ContextMode::Command {
                    shell.capture_event();
                    return;
                }
                if let input_method::Event::Commit(text) = input_event
                    && !text.is_empty()
                    && self.on_input_intent.is_some()
                    && self.on_context.is_some()
                {
                    state.composition = None;
                    state.interaction_sequence = state.interaction_sequence.saturating_add(1);
                    state.controlled_pending = true;
                    self.emit_context_commit(state, text, shell);
                    return;
                }
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
                        let local_y = (position.y - track_y).clamp(0.0, track_height);
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
                        if !self.context_mode.active()
                            && let Some(action) =
                                link_action_at(&self.view, &state.link_spans, byte)
                        {
                            self.emit(shell, action);
                            shell.capture_event();
                            shell.request_redraw();
                            return;
                        }
                        state.focused = true;
                        state.ime_active = true;
                        state.selecting = true;
                        let click =
                            mouse::Click::new(position, mouse::Button::Left, state.last_click);
                        state.last_click = Some(click);
                        state.select_click(
                            &self.view.text,
                            byte,
                            click.kind(),
                            state.modifiers.shift(),
                            state.modifiers.command() || state.modifiers.control(),
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
                        let local_y = (position.y - track_y - drag.grab_offset).clamp(0.0, travel);
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
                    let byte = state.hit_test_cached(
                        &self.view.text,
                        self.view.revision,
                        state.viewport,
                        position.x - PAD_X,
                        position.y - PAD_Y,
                        self.text_wrap_width(layout.bounds().width),
                    );
                    state.extend_pointer_selection(&self.view.text, byte);
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
                state.mod_drag = None;
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
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if state.focused && state.composition.is_none() =>
            {
                // Widget-owned interceptions only (X04/X05: one captured-intent
                // path for everything else). The autocomplete popup consumes
                // its navigation keys; plain-edit Escape collapses the set
                // (D-6 via E-14: additional selections first, then a range to
                // its head). Both are selection-local; nothing here types.
                let completion_control = matches!(
                    key,
                    Key::Named(Named::Enter | Named::ArrowUp | Named::ArrowDown | Named::Escape)
                ) && !modifiers.control()
                    && !modifiers.alt()
                    && !modifiers.logo();
                if completion_control && self.autocomplete_key(state, &key, shell) {
                    shell.capture_event();
                    shell.request_redraw();
                } else if matches!(key, Key::Named(Named::Escape))
                    && !completion_control
                    && self.context_mode == flavite_edit_context::ContextMode::Off
                    && !modifiers.control()
                    && !modifiers.alt()
                    && !modifiers.logo()
                    && state.selections.escape_collapse(&self.view.text)
                {
                    shell.capture_event();
                    shell.request_redraw();
                }
            }
            _ => {}
        }
        if self.input_enabled
            && state.focused
            && state.ime_active
            && self.context_mode != flavite_edit_context::ContextMode::Command
        {
            let width = self.text_wrap_width(layout.bounds().width);
            let caret_byte = state.selections.caret();
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
        // Modifier/unsupported key presses do not acknowledge a caret.
        // While captured edits await redraw their snapshot may still have
        // the old revision, which would break the host's ordered input chain.
        let explicit_interaction = matches!(
            event,
            iced::Event::Mouse(
                mouse::Event::ButtonPressed(mouse::Button::Left)
                    | mouse::Event::ButtonReleased(mouse::Button::Left)
                    | mouse::Event::WheelScrolled { .. }
            )
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
    state.candidate_matches(&widget.candidates).len()
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

pub(crate) fn shape_text(text: &str, width: f32, metrics: TypeMetrics) -> ShapingBuffer {
    FONT_SYSTEM.with(|font_system| {
        let mut font_system = font_system.borrow_mut();
        let mut buffer = ShapingBuffer::new(
            &mut font_system,
            Metrics::new(metrics.font_size, metrics.line_height),
        );
        buffer.set_size(&mut font_system, Some(width), None);
        buffer.set_wrap(&mut font_system, cosmic_text::Wrap::WordOrGlyph);
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

/// Geometry is derived from the exact shaped glyph clusters used for text.
/// Joining only visually adjacent rectangles avoids boxing unrelated text in
/// mixed-direction rows. Only the shaped viewport is inspected.
fn command_preview_rects(
    visible: &str,
    source_start: usize,
    runs: &[cosmic_text::LayoutRun<'_>],
    range: std::ops::Range<usize>,
    metrics: TypeMetrics,
) -> Vec<Rectangle> {
    let mut result = Vec::new();
    for run in runs {
        let line_start = source_start + line_start_byte(visible, run.line_i);
        let mut spans: Vec<_> = run
            .glyphs
            .iter()
            .filter(|glyph| {
                line_start + glyph.end > range.start && line_start + glyph.start < range.end
            })
            .map(|glyph| (glyph.x, glyph.x + glyph.w.max(1.0)))
            .collect();
        spans.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
        let mut merged: Option<(f32, f32)> = None;
        for (left, right) in spans {
            if let Some((_, end)) = &mut merged
                && left <= *end + 0.5
            {
                *end = end.max(right);
            } else {
                if let Some((start, end)) = merged {
                    result.push(Rectangle::new(
                        Point::new(start, run.line_top),
                        Size::new(end - start, metrics.line_height),
                    ));
                }
                merged = Some((left, right));
            }
        }
        if let Some((start, end)) = merged {
            result.push(Rectangle::new(
                Point::new(start, run.line_top),
                Size::new(end - start, metrics.line_height),
            ));
        }
    }
    // Newlines have no glyph. An outlined cell makes an empty-line Cut visible
    // and distinguishes copying/cutting a line with its terminator from Delete.
    for (local, ch) in visible.char_indices() {
        if ch == '\n' && range.contains(&(source_start + local)) {
            let (x, y) = caret_position(runs, visible, local);
            result.push(Rectangle::new(
                Point::new(x, y),
                Size::new(metrics.font_size * 0.6, metrics.line_height),
            ));
        }
    }
    result
}

pub(crate) fn caret_position(
    runs: &[cosmic_text::LayoutRun<'_>],
    text: &str,
    byte: usize,
) -> (f32, f32) {
    for (index, run) in runs.iter().enumerate() {
        let line_start = line_start_byte(text, run.line_i);
        let line_end = line_start + run.text.len();
        if byte < line_start || byte > line_end {
            continue;
        }
        // `run.text` covers the entire source paragraph, including rows
        // after this one. A caret on a wrapped row must skip earlier rows.
        if runs.get(index + 1).is_some_and(|next| {
            next.line_i == run.line_i
                && next
                    .glyphs
                    .iter()
                    .map(|glyph| glyph.start)
                    .min()
                    .is_some_and(|start| byte >= line_start + start)
        }) {
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
    use crate::viewport::{next_grapheme_boundary, prev_grapheme_boundary};
    use flavite_buffer::state::BufferState;
    use flavite_syntax::text::{Span, TextByte};

    #[test]
    fn command_outlines_cover_only_the_affected_shaped_glyphs_and_visible_newlines() {
        let text = "abcdef\n\n雪e\u{301}";
        let metrics = TypeMetrics::default();
        let shaped = shape_text(text, 300.0, metrics);
        let runs = shaped.layout_runs().collect::<Vec<_>>();
        let rects = command_preview_rects(text, 100, &runs, 102..104, metrics);
        assert!(!rects.is_empty());
        for run in &runs {
            let line = 100 + line_start_byte(text, run.line_i);
            for glyph in run.glyphs {
                let center = Point::new(
                    glyph.x + glyph.w / 2.0,
                    run.line_top + metrics.line_height / 2.0,
                );
                assert_eq!(
                    rects.iter().any(|rect| rect.contains(center)),
                    line + glyph.end > 102 && line + glyph.start < 104
                );
            }
        }
        let blank_line = command_preview_rects(text, 100, &runs, 107..108, metrics);
        assert_eq!(
            blank_line.len(),
            1,
            "an empty line's newline has a visible cell"
        );
        assert!(blank_line[0].width > 0.0);
        assert!(
            command_preview_rects(text, 100, &runs, 0..50, metrics).is_empty(),
            "offscreen source does not create paint primitives"
        );

        let wrapped = shape_text("one two three four five", 50.0, metrics);
        let runs = wrapped.layout_runs().collect::<Vec<_>>();
        let rects = command_preview_rects("one two three four five", 0, &runs, 0..23, metrics);
        assert!(
            rects.iter().any(|rect| rect.y > 0.0),
            "a wrapped line's outline follows its actual rows"
        );
    }
    use iced::advanced::widget::Operation as _;

    #[test]
    fn new_source_receipt_restores_focus_and_selection_after_an_old_pointer_release() {
        let original = EditorView::from(&test_view("abcdef", 77));
        let mut state = EditorState::default();
        let snapshot = |revision, sequence, at, focused| EditorInteractionSnapshot {
            buffer_id: 77,
            revision,
            sequence,
            caret: at,
            selection: (at, at),
            selections: vec![[0, at.min(at), at]],
            viewport_anchor: 0,
            focused,
            composing: false,
        };
        EditorWidget::from_view(original.clone(), |action| action)
            .with_initial_interaction(snapshot(original.revision, 40, 0, false))
            .synchronize(&mut state);
        state.interaction_sequence = 100;
        state.focused = false;
        let mut restored = original;
        restored.revision += 1;
        restored.text = "xyabcdef".into();
        let widget = EditorWidget::from_view(restored.clone(), |action| action)
            .with_initial_interaction(snapshot(restored.revision, 41, 4, true));
        widget.synchronize(&mut state);
        assert!(state.focused);
        assert_eq!(state.selections.caret(), 4);
        assert_eq!(
            state.interaction_sequence, 100,
            "native sequence cannot go backwards"
        );
        state.selections.replace_with("xyabcdef", &[(6, 6)], 0);
        state.focused = false;
        widget.synchronize(&mut state);
        assert_eq!(
            state.selections.caret(),
            6,
            "resolver-only refresh cannot overwrite newer pointer state"
        );
        assert!(!state.focused);
    }

    #[test]
    fn word_and_line_clicks_extend_as_units_in_both_directions() {
        let text = "one café three\nnext line\n";
        let mut state = EditorState::default();
        state.select_click(text, 6, mouse::click::Kind::Double, false, false);
        assert_eq!(state.selection(), (4, 9));
        state.extend_pointer_selection(text, 11);
        assert_eq!(state.selection(), (4, 15));
        state.extend_pointer_selection(text, 1);
        assert_eq!(state.selection(), (0, 9));
        state.select_click(text, 6, mouse::click::Kind::Triple, false, false);
        assert_eq!(state.selection(), (0, 16));
        state.extend_pointer_selection(text, 18);
        assert_eq!(state.selection(), (0, text.len()));
        state.select_click(text, 2, mouse::click::Kind::Single, false, false);
        state.select_click(text, 18, mouse::click::Kind::Single, true, false);
        assert_eq!(state.selection(), (2, 18));
    }

    #[test]
    fn completion_dismisses_when_its_source_prefix_or_selection_changes() {
        let mut state = EditorState::default();
        for text in [
            "first word",
            "[[target]]",
            "[[target\nnext",
            "[[target|label",
            "[[target#heading",
        ] {
            state.open_autocomplete(0);
            state
                .selections
                .replace_with(text, &[(text.len(), text.len())], 0);
            state.refresh_completion(text);
            assert!(state.autocomplete_start.is_none(), "{text:?}");
        }
        state.open_autocomplete(0);
        state.selections.replace_with("[[café", &[(5, 5)], 0);
        state.refresh_completion("[[café");
        // A caret at a UTF-8 scalar boundary inside the target remains valid.
        assert_eq!(state.autocomplete_query, "caf");
        state.selections.replace_with("[[café", &[(0, 0)], 0);
        state.refresh_completion("[[café");
        assert!(state.autocomplete_start.is_none());
    }

    #[test]
    fn editor_keymap_covers_vertical_and_word_actions() {
        // Workspace splitting uses Ctrl+Alt+arrows and must reach the shell.
        assert_eq!(
            key_action(
                &Key::Named(Named::ArrowRight),
                Modifiers::CTRL | Modifiers::ALT,
                None
            ),
            KeyAction::Ignore
        );
        for (key, expected) in [
            (Named::ArrowUp, KeyAction::Up),
            (Named::ArrowDown, KeyAction::Down),
            (Named::PageUp, KeyAction::PageUp),
            (Named::PageDown, KeyAction::PageDown),
        ] {
            assert_eq!(
                key_action(&Key::Named(key), Modifiers::SHIFT, None),
                expected
            );
        }
        let word = if cfg!(target_os = "macos") {
            Modifiers::ALT
        } else {
            Modifiers::CTRL
        };
        for (key, expected) in [
            (Named::ArrowLeft, KeyAction::PreviousWord),
            (Named::ArrowRight, KeyAction::NextWord),
            (Named::Backspace, KeyAction::BackspaceWord),
            (Named::Delete, KeyAction::DeleteWord),
        ] {
            assert_eq!(
                key_action(&Key::Named(key), word | Modifiers::SHIFT, None),
                expected
            );
        }
    }

    #[test]
    fn synchronization_preserves_metrics_and_resolver_snapshot_until_changed() {
        let view = EditorView::from(&test_view("[[target]]", 7));
        let verdicts = Arc::new(HashMap::from([("target".into(), LinkResolution::Resolved)]));
        let widget = EditorWidget::from_view(view.clone(), |action| action)
            .with_view_key(77, 1)
            .with_type_metrics(TypeMetrics::for_font_size(24.0))
            .with_link_resolutions(verdicts.clone());
        let mut state = EditorState::default();
        widget.synchronize(&mut state);
        assert_eq!(state.type_metrics.font_size, 24.0);
        assert_eq!(state.link_spans[0].resolution, LinkResolution::Resolved);
        for _ in 0..10 {
            widget.synchronize(&mut state);
            assert!(Arc::ptr_eq(
                state.resolution_snapshot.as_ref().unwrap(),
                &verdicts
            ));
            assert_eq!(state.link_spans[0].resolution, LinkResolution::Resolved);
        }
        let empty = EditorWidget::from_view(view, |action| action).with_view_key(77, 1);
        empty.synchronize(&mut state);
        assert_eq!(state.link_spans[0].resolution, LinkResolution::Unresolved);
    }

    #[test]
    fn restoring_an_unmeasured_editor_preserves_the_host_scroll_anchor() {
        let view = long_view(40);
        let mut state = EditorState::default();
        state.reconcile(&view, &HashMap::new());
        let caret = view.text.find("line 8").unwrap();
        state.restore_interaction(
            &view.text,
            EditorInteractionSnapshot {
                sequence: 1,
                buffer_id: view.buffer_id,
                revision: view.revision,
                caret,
                selection: (caret, caret),
                selections: vec![[0, caret, caret]],
                viewport_anchor: 0,
                focused: true,
                composing: false,
            },
        );
        assert_eq!(
            state.viewport.first_line, 0,
            "sidebar docking must not scroll a fresh one-row viewport to the caret"
        );
        assert_eq!(state.selections.caret(), caret);
    }

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
        let far = "line 150\n".len() * 150;
        state.selections.replace_with(&view.text, &[(far, far)], 0);
        state.reconcile(&view, &HashMap::new());
        assert!(state.viewport.first_line >= 140);
        assert_eq!(state.last_ensured_caret, state.selections.caret());
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
        let mut state = EditorState::default();
        state.selections.replace_with("hello", &[(2, 2)], 0);
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
        assert_eq!(state.selections.caret(), 2 + "日本語".len());
        assert!(!state.is_composing());
    }

    #[test]
    fn cancelled_ime_composition_never_changes_the_document() {
        let view = EditorView::from(&test_view("hello", 12));
        let mut state = EditorState {
            focused: true,
            ime_active: true,
            ..EditorState::default()
        };
        state.selections.replace_with("hello", &[(3, 3)], 0);
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
        assert_eq!(state.selections.caret(), 3);

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
        assert_eq!(state.selections.caret(), 3 + "仮名".len());
        assert!(!state.is_composing());
    }

    #[test]
    fn host_operation_observes_the_focused_editor_caret_and_composition() {
        let view = EditorView::from(&test_view("text", 12));
        let mut state = EditorState {
            buffer_id: Some(view.buffer_id),
            revision: Some(view.revision),
            selections: {
                let mut s = EditorSelections::default();
                s.replace_with("text", &[(1, 2)], 0);
                s
            },
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
            focused: false,
            ..EditorState::default()
        };
        state.selections.replace_with("text", &[(2, 2)], 0);
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
            focused: false,
            ..EditorState::default()
        };
        state.selections.replace_with("text", &[(3, 1)], 0);
        let mut stale = focus_editor(view.buffer_id, view.revision + 1, None);
        stale.custom(None, Rectangle::with_size(Size::ZERO), &mut state);
        assert!(!state.focused);
        assert!(matches!(
            stale.finish(),
            iced::advanced::widget::operation::Outcome::Some(None)
        ));

        let mut current = focus_editor(view.buffer_id, view.revision, None);
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
            CompletionCandidate::new("notes/inbox", "Inbox"),
            CompletionCandidate::new("Ideas", "Ideas"),
        ]));
        let mut state = EditorState::default();
        state
            .selections
            .replace_with(&view.text, &[(view.text.len(), view.text.len())], 0);
        state.open_autocomplete(0);
        state.autocomplete_query = "In".into();
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        assert!(widget.autocomplete_key(&mut state, &Key::Named(Named::Enter), &mut shell));
        drop(shell);
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0], EditorAction::Complete {
            buffer_id: 97, expected_revision: 11, start: 0, end: 4,
            prefix, candidate,
        } if prefix == "[[In" && candidate.target == "notes/inbox"));
        assert_eq!(
            state.selections.caret(),
            4,
            "completion waits for the semantic acknowledgement"
        );
        assert!(state.autocomplete_start.is_none());
    }

    #[test]
    fn autocomplete_create_row_opens_unresolved_note_for_creation() {
        let view = test_view("[[New", 103);
        let widget = EditorWidget::new(&view, |action| action)
            .with_candidates(Arc::from([CompletionCandidate::new("Ideas", "Ideas")]));
        let mut state = EditorState::default();
        state
            .selections
            .replace_with(&view.text, &[(view.text.len(), view.text.len())], 0);
        state.open_autocomplete(0);
        state.autocomplete_query = "New note".into();
        state.autocomplete_selected = 0; // no matching notes, so the create row is selected
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);

        assert!(widget.autocomplete_key(&mut state, &Key::Named(Named::Enter), &mut shell));
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
        let mut candidate = CompletionCandidate::new("notes/concepts", "Concepts");
        candidate.stable_id = Some("01J9ZK".into());
        let format = |candidate: &CompletionCandidate, stable: bool| {
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
        assert_eq!(prev_grapheme_boundary(text, TextByte(end)).0, "a🧑🏽‍💻".len());
        assert_eq!(next_grapheme_boundary(text, TextByte(1)).0, "a🧑🏽‍💻".len());
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
            selections: vec![[0, caret, caret]],
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
    fn pointer_toggle_and_escape_collapse_follow_the_set_vocabulary() {
        // E-14 pointer vocabulary + D-6 Escape collapse, exercised through
        // the same state methods the event path uses.
        let text = "one two three";
        let mut state = EditorWidget::<EditorAction>::state_for_test();
        state.select_click(text, 1, mouse::click::Kind::Single, false, false);
        assert_eq!(state.selection(), (1, 1));
        // Mod-click adds a second caret; the primary stays first (canonical
        // order), and a second toggle on the same spot removes it again.
        assert!(state.selections.toggle_member(text, 5, (5, 5)), "added");
        assert_eq!(state.selections.len(), 2);
        assert!(!state.selections.toggle_member(text, 5, (5, 5)), "removed");
        assert_eq!(state.selections.len(), 1);
        // The last member is never removed by a toggle.
        assert!(!state.selections.toggle_member(text, 1, (1, 1)));
        assert_eq!(state.selections.len(), 1);
        // Escape collapse: a range folds to its head; a caret is a no-op.
        state.select_click(text, 4, mouse::click::Kind::Double, false, false);
        assert_eq!(state.selection(), (4, 7));
        assert!(state.selections.escape_collapse(text));
        assert_eq!(state.selection(), (7, 7));
        assert!(!state.selections.escape_collapse(text));
    }

    #[test]
    fn escape_collapse_drops_additional_members_first() {
        let text = "one two three";
        let mut state = EditorWidget::<EditorAction>::state_for_test();
        state.select_click(text, 0, mouse::click::Kind::Double, false, false);
        assert!(state.selections.toggle_member(text, 8, (8, 8)));
        assert_eq!(state.selections.len(), 2);
        // First Escape: additional members drop, the primary range stays.
        assert!(state.selections.escape_collapse(text));
        assert_eq!(state.selections.len(), 1);
        assert_eq!(state.selection(), (0, 3));
        // Second Escape: the remaining range folds to its head.
        assert!(state.selections.escape_collapse(text));
        assert_eq!(state.selection(), (3, 3));
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
        assert_eq!(state.selections.caret(), expected_byte);
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
            ..EditorState::default()
        };
        state.selections.replace_with("long text", &[(8, 4)], 0);

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
        assert_eq!(state.selections.caret(), 0);
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
        let text = "one\ntwo\nthree\nfour\nfive";
        let mut state = EditorState {
            viewport: Viewport {
                visible_lines: 3,
                ..Viewport::default()
            },
            ..EditorState::default()
        };
        state
            .selections
            .replace_with(text, &[(text.len(), text.len())], 0);
        state.ensure_caret_visible(text);
        assert_eq!(state.viewport.first_line, 2);

        state.selections.replace_with(text, &[(0, 0)], 0);
        state.ensure_caret_visible(text);
        assert_eq!(state.viewport.first_line, 0);
    }
}
