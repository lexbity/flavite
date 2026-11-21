//! Custom iced editor widget over a revisioned, immutable API view.

use flavite_api::CompletionCandidate;

use crate::keymap::{KeyAction, key_action};
use crate::links::{LinkResolution, LinkSpan, match_candidates, resolve_link};
use crate::navigation::VerticalMotion;
use crate::selections::EditorSelections;
use crate::surface::{EditorAction, EditorSurface, EditorView, LinkNavigation};
use crate::viewport::{Viewport, clamp_char_boundary};
use cosmic_text::{Attrs, Buffer as ShapingBuffer, BufferLine, FontSystem, Metrics, Shaping};
#[cfg(feature = "test-support")]
use flavite_api::BufferView;
use flavite_buffer::chunks::DocumentText;
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
    /// Muted selection fill for a surviving multi-member set while Context
    /// Mode is Off (FEUX-0002 FR-13): derived from the same semantic token
    /// with a lower alpha, never a new palette entry.
    pub selection_muted: Color,
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
            selection_muted: Color::from_rgba(0.24, 0.42, 0.67, 0.22),
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
    /// Match-selection draft overlay (FEUX-0002 X06/E-11): outline-only,
    /// never a committed representation.
    match_preview: Option<Arc<[(usize, usize)]>>,
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
    capture_sequence: Option<Arc<std::sync::atomic::AtomicU64>>,
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
            match_preview: None,
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
            capture_sequence: None,
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

    /// Use the lease-owned sequence across remounted widgets of this view.
    #[must_use]
    pub fn with_capture_sequence(mut self, sequence: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.capture_sequence = Some(sequence);
        self
    }

    fn advance_capture(&self, state: &mut EditorState) {
        let floor = state
            .interaction_sequence
            .max(self.initial_interaction.as_ref().map_or(0, |s| s.sequence));
        state.interaction_sequence = self.capture_sequence.as_ref().map_or_else(
            || floor.saturating_add(1),
            |sequence| {
                use std::sync::atomic::Ordering;
                sequence
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                        Some(old.max(floor).saturating_add(1))
                    })
                    .unwrap()
                    .max(floor)
                    .saturating_add(1)
            },
        );
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

    /// Paint a match-selection draft overlay (FEUX-0002 E-11): outline-only
    /// spans that never mutate the committed set. `None` clears it.
    #[must_use]
    pub fn with_match_preview(mut self, spans: Option<Arc<[(usize, usize)]>>) -> Self {
        self.match_preview = spans;
        self
    }

    fn emit_context(
        &self,
        state: &mut EditorState,
        event: crate::context::ContextEvent,
        shell: &mut Shell<'_, Message>,
    ) {
        if let Some(callback) = &self.on_context {
            self.advance_capture(state);
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
                    clipboard_acquired: false,
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
                let text = state.materialized_text(&self.view);
                state.restore_interaction(&self.view.text, &text, interaction);
                state.interaction_sequence = state.interaction_sequence.max(captured_sequence);
            } else if self.on_input_intent.is_some()
                && state.controlled_pending
                && interaction.sequence > state.controlled_ack
            {
                let captured_sequence = state.interaction_sequence;
                let incoming_sequence = interaction.sequence;
                state.interaction_sequence = incoming_sequence.saturating_sub(1);
                let text = state.materialized_text(&self.view);
                state.restore_interaction(&self.view.text, &text, interaction);
                state.interaction_sequence = state.interaction_sequence.max(captured_sequence);
                state.controlled_ack = incoming_sequence;
                state.controlled_pending = incoming_sequence < captured_sequence;
                if text
                    .get(..state.selections.caret())
                    .is_some_and(|text| text.ends_with("[["))
                {
                    state.open_autocomplete(state.selections.caret() - 2);
                }
            } else {
                let text = state.materialized_text(&self.view);
                state.restore_interaction(&self.view.text, &text, interaction);
            }
        }
        // RFC-0011 S4: the completion refresh materializes only when the
        // popup is actually open (a user-paced overlay), never on the plain
        // typing path.
        if state.autocomplete_start.is_some() {
            let text = state.materialized_text(&self.view);
            state.refresh_completion(&text);
        }
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
                    let text = state.materialized_text(&self.view);
                    self.emit(
                        shell,
                        EditorAction::Complete {
                            buffer_id: self.view.buffer_id,
                            expected_revision: self.view.revision,
                            start,
                            end: state.selections.caret(),
                            prefix: text[start..state.selections.caret()].to_owned(),
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

/// Documents at or above this size skip markdown structure parsing entirely
/// (plain mode: no spans; RFC-0008 FR-9). Flag-independent; also the gate
/// the paged-buffer RFC inherits.
pub(crate) const HUGE_TEXT_THRESHOLD: usize = 64 * 1024 * 1024;

/// The pulldown bridge: the shipped default structure source (RFC-0008
/// Appendix C). Same parser and converter that the pre-RFC whole-document
/// scanning path used — parity-proven against the tree-sitter path (AC-5)
/// — so default routing preserves known-good spans and latency at every
/// document size under the plain-mode gate.
fn bridge_structure_spans(text: &str) -> (Vec<LinkSpan>, Vec<flavite_syntax::ScannedSpan>) {
    let Ok(parsed) = flavite_syntax::pulldown_glue::parse_normalized(text) else {
        return (Vec::new(), Vec::new());
    };
    crate::links::spans_from_pulldown(parsed)
}

/// The incremental structure engine path (RFC-0008 D7-C) — retained only as
/// a test/bench seam: the widget no longer routes through it (RFC-0011 S4),
/// and the engine is deleted in RFC-0011 S6. `engine_structure_spans_for_perf`
/// drives it directly for the S8-T2/S8-T4 parity-and-cost measurements.
fn engine_structure_spans(
    text: &str,
    hint: Option<flavite_syntax::EditRange>,
    lines: &[usize],
    engine: &mut flavite_syntax::StructureEngine,
) -> (Vec<LinkSpan>, Vec<flavite_syntax::ScannedSpan>) {
    // D10: render never blocks on structure — a degraded or failed sync
    // falls back to the bridge, never an error path.
    match engine.sync(text, lines, hint) {
        Ok(spans) => crate::links::spans_from_structure(spans),
        Err(_) => bridge_structure_spans(text),
    }
}

/// Produce the structure spans for a revision change (RFC-0008 S7/S8,
/// RFC-0011 S4):
///
/// * **Plain mode** (FR-9): documents ≥ [`HUGE_TEXT_THRESHOLD`] skip all
///   markdown parsing — no pulldown, no tree-sitter, no spans. Editing,
///   undo, find, and save are unaffected.
/// * **Default**: the pulldown bridge ([`bridge_structure_spans`]) — the
///   pre-RFC-equivalent structure source, at every size under the plain
///   gate.
///
/// RFC-0011 S4: the tree-sitter engine is no longer routed here (its
/// incremental reparses do not localize — RFC-0008 Appendix C measured —
/// and it is retired in S6). The bridge is the transitional whole-document
/// producer until the segment cache lands in S5; S4 keeps this phase at
/// exactly the shipped HEAD cost (one O(n) parse per revision below the
/// plain gate) — `// S5 removes the last O(n) keystroke cost` marks the
/// call site in [`EditorState::reconcile_view`].
fn structure_spans(text: &str) -> (Vec<LinkSpan>, Vec<flavite_syntax::ScannedSpan>) {
    if text.len() >= HUGE_TEXT_THRESHOLD {
        (Vec::new(), Vec::new())
    } else {
        bridge_structure_spans(text)
    }
}

/// Test seam for the end-to-end keystroke bench (S8-T5): runs the exact
/// same dispatch the widget uses on a revision change — plain-mode gate,
/// pulldown bridge. Public only so `tests/` integration benches can measure
/// the shipped configuration; not part of the editor's public API surface.
#[doc(hidden)]
#[must_use]
pub fn structure_spans_for_perf(text: &str) -> (Vec<LinkSpan>, Vec<flavite_syntax::ScannedSpan>) {
    structure_spans(text)
}

/// Test seam for the S8-T5 bench: the engine path, measured directly and
/// deterministically (independent of the env flag).
#[doc(hidden)]
#[must_use]
pub fn engine_structure_spans_for_perf(
    text: &str,
    hint: Option<flavite_syntax::EditRange>,
    lines: &[usize],
    engine: &mut flavite_syntax::StructureEngine,
) -> (Vec<LinkSpan>, Vec<flavite_syntax::ScannedSpan>) {
    engine_structure_spans(text, hint, lines, engine)
}

/// Test seam for the S8-T5 bench: the bridge path, measured directly and
/// deterministically.
#[doc(hidden)]
#[must_use]
pub fn bridge_structure_spans_for_perf(
    text: &str,
) -> (Vec<LinkSpan>, Vec<flavite_syntax::ScannedSpan>) {
    bridge_structure_spans(text)
}

/// Widget-local interaction state. Text and mutations remain Session-owned.
#[derive(Debug, Default)]
pub struct EditorState {
    view_key: Option<(u128, u64)>,
    buffer_id: Option<u64>,
    revision: Option<u64>,
    link_spans: Vec<LinkSpan>,
    decorations: Vec<flavite_syntax::ScannedSpan>,
    /// One materialized copy per view revision, shared by the structure
    /// bridge (RFC-0011 S4 transitional — S5 removes it) and the user-paced
    /// interaction paths (selection/word-bound/autocomplete APIs that take
    /// `&str`). It is populated lazily, never on the plain-mode typing
    /// path; all shaping, line geometry, and paint queries go to the tree.
    /// `RefCell` so the read-only frame paths (`draw`) share the cache with
    /// the mutations (`layout`, `update`).
    materialized_text: std::cell::RefCell<Option<(u64, std::sync::Arc<str>)>>,
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
    /// Persistent shaping of the visible viewport band (RFC-0008 S2; see
    /// [`ShapingCacheV2`]).
    shaping: Option<ShapingCacheV2>,
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

/// Font/line-height bits, so a preference change reshapes immediately.
fn shaping_type_bits(metrics: TypeMetrics) -> u64 {
    (metrics.font_size.to_bits() as u64) << 32 | u64::from(metrics.line_height.to_bits())
}

/// The shaped slice around a viewport: one viewport of margin above and
/// below, clamped to the document. Motion within the band never reshapes;
/// the band moves (one reshape) only when the viewport leaves it.
fn band_window(viewport: Viewport, line_count: usize) -> (usize, usize) {
    let visible = viewport.visible_lines.max(1);
    let start = viewport
        .first_line
        .saturating_sub(visible)
        .min(line_count.saturating_sub(1));
    let end = viewport
        .first_line
        .saturating_add(visible.saturating_mul(2))
        .min(line_count);
    (start, end.saturating_sub(start).max(1))
}

/// Logical-line start byte offsets within a slice, using the same `\n`
/// paragraph split the editor's line index uses. Shaped `line_i` and
/// `Cursor.line` indexes resolve through these; an index past the `\n`
/// paragraph count (possible when a shaper splits further, e.g. on
/// paragraph-separator characters) falls back to the slice end, matching the
/// historical `line_start_byte` behavior.
/// Band-local only: the slice is the band text (O(band)), never the whole
/// document (RFC-0011 S4 — the full-document line scan is gone).
pub(crate) fn shape_line_starts(visible: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    if !visible.is_empty() {
        starts.push(0);
        starts.extend(
            visible
                .bytes()
                .enumerate()
                .filter(|&(_, byte)| byte == b'\n')
                .map(|(at, _)| at + 1),
        );
    }
    starts
}

/// The persistent shaping state (RFC-0008 S2, D2): one cosmic-text [`Buffer`]
/// owned per widget instance, with the band's per-line fingerprints and local
/// line starts. Keystrokes splice only changed `BufferLine`s (FR-2 explains);
/// full rebuilds happen only on width/type change, band-identity failure,
/// or self-heal (D10).
struct ShapingCacheV2 {
    /// Persistent shaped buffer: `buffer.lines[i]` mirrors band line `i`.
    buffer: ShapingBuffer,
    /// The band's identity (window, source anchor, revision, band text).
    band: BandState,
    /// Wrap width at the last sync (`f32` bits).
    width_bits: u32,
    /// Font/line-height bits at the last sync.
    type_bits: u64,
    /// Per band-line `(byte_len, fingerprint)` (D1 ground-truth storage; the
    /// splice itself diffs exact line text).
    line_hashes: Vec<(usize, u64)>,
    /// Band-local logical-line start offsets within `band.visible`.
    line_starts: Vec<usize>,
    /// Test-only splice accounting (S2-T3: entering lines constructed once,
    /// exiting lines removed).
    #[cfg(test)]
    stats: SpliceStats,
}

/// The band state (RFC-0008 §5.2 `BandState`).
#[derive(Debug)]
struct BandState {
    /// First logical line index of the band in the document.
    band_start: usize,
    /// Number of logical lines in the band.
    band_lines: usize,
    /// Byte offset of the band's first line in the source text.
    source_start: usize,
    /// The revision the band currently mirrors.
    revision: u64,
    /// The band text (ownership — the source `Arc<str>` is not borrowed).
    visible: String,
}

/// Test-only splice accounting (S2-T3).
#[cfg(test)]
#[derive(Debug, Default)]
struct SpliceStats {
    /// BufferLines newly constructed by the last sync (entering lines).
    constructed: usize,
    /// BufferLines removed by the last sync (exiting lines).
    removed: usize,
}

/// One resolved shaping: the shaped band buffer, the band text, its byte
/// offset in the source, the band's first logical line, and per-line start
/// offsets within the band text.
struct ShapedView<'a> {
    buffer: &'a ShapingBuffer,
    visible: &'a str,
    source_start: usize,
    band_start: usize,
    line_starts: &'a [usize],
}

impl ShapedView<'_> {
    /// Byte offset of logical line `line` within the band text.
    fn line_offset(&self, line: usize) -> usize {
        self.line_starts
            .get(line)
            .copied()
            .unwrap_or(self.visible.len())
    }

    /// Row offset (pixels) of the viewport's first line within the band: the
    /// wrapped rows of band lines above the viewport start. Rows align with
    /// logical-line boundaries, so the first run at or after the viewport's
    /// first line sits exactly at the viewport's top.
    fn paint_offset(&self, viewport: Viewport) -> f32 {
        let first_local = viewport.first_line.saturating_sub(self.band_start);
        self.buffer
            .layout_runs()
            .find(|run| run.line_i >= first_local)
            .map_or(0.0, |run| run.line_top)
    }
}

impl std::fmt::Debug for ShapingCacheV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShapingCacheV2")
            .field("band", &self.band)
            .field("width_bits", &self.width_bits)
            .field("type_bits", &self.type_bits)
            .field("line_hashes.len", &self.line_hashes.len())
            .field("line_starts.len", &self.line_starts.len())
            .finish_non_exhaustive()
    }
}

/// The monospace attribute list every band line is shaped with (matches
/// [`shape_text`]).
fn mono_attrs_list() -> cosmic_text::AttrsList {
    cosmic_text::AttrsList::new(&Attrs::new().family(cosmic_text::Family::Monospace))
}

/// Split one whole logical line slice into its content (terminator stripped)
/// and its line ending — mirroring `cosmic_text::LineIter`, which stores the
/// terminator separately from the `BufferLine` text.
fn split_line_content_and_ending(slice: &str) -> (&str, cosmic_text::LineEnding) {
    if let Some(content) = slice.strip_suffix("\r\n") {
        (content, cosmic_text::LineEnding::CrLf)
    } else if let Some(content) = slice.strip_suffix("\n\r") {
        (content, cosmic_text::LineEnding::LfCr)
    } else if let Some(content) = slice.strip_suffix('\n') {
        (content, cosmic_text::LineEnding::Lf)
    } else if let Some(content) = slice.strip_suffix('\r') {
        (content, cosmic_text::LineEnding::Cr)
    } else {
        (slice, cosmic_text::LineEnding::None)
    }
}

/// Per-band-line shape spans: `(first-row top, wrapped height)` for every
/// logical line (RFC-0008 S3). Lines without shaped runs (empty/trailing)
/// fall back to one row of `fallback_height`.
fn band_line_spans(
    buffer: &ShapingBuffer,
    line_count: usize,
    fallback_height: f32,
) -> Vec<(f32, f32)> {
    let mut tops = vec![f32::NAN; line_count];
    let mut bottoms = vec![0.0_f32; line_count];
    for run in buffer.layout_runs() {
        let index = run.line_i;
        if index < line_count {
            if tops[index].is_nan() {
                tops[index] = run.line_top;
            }
            let bottom = run.line_top + run.line_height;
            if bottom > bottoms[index] {
                bottoms[index] = bottom;
            }
        }
    }
    let mut cursor = 0.0_f32;
    let mut spans = Vec::with_capacity(line_count);
    for index in 0..line_count {
        if !tops[index].is_nan() {
            cursor = tops[index];
        }
        let bottom = if bottoms[index] > 0.0 {
            bottoms[index]
        } else {
            cursor + fallback_height
        };
        spans.push((cursor, (bottom - cursor).max(fallback_height)));
    }
    spans
}

/// Indices of logical band lines intersecting the paint window `[0,
/// paint_height)` (viewport coordinates; FR-3 whole-line granularity).
fn painted_line_indices(spans: &[(f32, f32)], paint_offset: f32, paint_height: f32) -> Vec<usize> {
    spans
        .iter()
        .enumerate()
        .filter(|&(_, &(top, height))| {
            let top_viewport = top - paint_offset;
            top_viewport < paint_height && top_viewport + height > 0.0
        })
        .map(|(index, _)| index)
        .collect()
}

/// One band line's paint content: the line text WITHOUT its terminator (a
/// trailing newline inside a paragraph would add a blank row).
fn band_line_content(visible: &str, starts: &[usize], index: usize) -> String {
    let start = starts
        .get(index)
        .copied()
        .unwrap_or(visible.len())
        .min(visible.len());
    let end = starts
        .get(index + 1)
        .copied()
        .unwrap_or(visible.len())
        .min(visible.len());
    let (content, _) = split_line_content_and_ending(&visible[start..end]);
    content.to_owned()
}

/// The `buffer.lines` length `Buffer::set_text` produces for band text
/// `visible` (mirrors cosmic-text's `LineIter` + trailing-empty-line rule).
fn expected_buffer_line_count(visible: &str) -> usize {
    let mut items = 0usize;
    let mut last_has_ending = false;
    for (_, ending) in cosmic_text::LineIter::new(visible) {
        items += 1;
        last_has_ending = ending != cosmic_text::LineEnding::None;
    }
    if items == 0 {
        1
    } else {
        items + usize::from(last_has_ending)
    }
}

/// One 64-bit line fingerprint (D1): byte length + `DefaultHasher` over the
/// line bytes.
fn line_fingerprint(slice: &str) -> (usize, u64) {
    (slice.len(), hash_bytes(slice.as_bytes()))
}

/// `DefaultHasher` over bytes (D1 ground-truth hashing).
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Fingerprints for every logical line of `visible` (line boundaries from
/// `starts`).
fn line_fingerprints(visible: &str, starts: &[usize]) -> Vec<(usize, u64)> {
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(visible.len());
            line_fingerprint(&visible[start..end])
        })
        .collect()
}

/// Whether a band covers a viewport (the cached band is reused while it does).
fn band_covers(band_start: usize, band_lines: usize, viewport: Viewport) -> bool {
    let viewport_end = viewport
        .first_line
        .saturating_add(viewport.visible_lines.max(1));
    band_start <= viewport.first_line && viewport_end <= band_start.saturating_add(band_lines)
}

/// Whether two bands share no overlap (touching edges are contiguous and can
/// be spliced).
fn bands_disjoint(a_start: usize, a_lines: usize, b: (usize, usize)) -> bool {
    let a_end = a_start.saturating_add(a_lines);
    b.0 >= a_end || a_start >= b.0.saturating_add(b.1)
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
    /// Index into `selections` of the primary member (canonical order and
    /// "primary first" cannot both be implicit; FEUX-0002 E-4/FR-4).
    pub primary: usize,
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
            let triples = state.selections.set().to_triples();
            let primary_id = state.selections.primary_id().as_raw();
            let primary = triples
                .iter()
                .position(|triple| triple[0] == primary_id as usize)
                .unwrap_or(0);
            self.0 = Some(EditorInteractionSnapshot {
                sequence: state.interaction_sequence,
                buffer_id,
                revision,
                caret: state.selections.caret(),
                selection: state.selection(),
                selections: triples,
                primary,
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
            let member = if byte < origin {
                (end, origin)
            } else {
                (origin, end)
            };
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

    fn interaction_snapshot(
        &self,
        text: Option<&DocumentText>,
    ) -> Option<EditorInteractionSnapshot> {
        let triples = self.selections.set().to_triples();
        let primary_id = self.selections.primary_id().as_raw();
        Some(EditorInteractionSnapshot {
            sequence: self.interaction_sequence,
            buffer_id: self.buffer_id?,
            revision: self.revision?,
            caret: self.selections.caret(),
            selection: self.selection(),
            primary: triples
                .iter()
                .position(|triple| triple[0] == primary_id as usize)
                .unwrap_or(0),
            selections: triples,
            viewport_anchor: text.map_or(0, |text| self.viewport_anchor(text)),
            focused: self.focused,
            composing: self.is_composing(),
        })
    }

    /// Install a host-controlled interaction acknowledgement. Lower or
    /// equal sequences and mismatched buffer/revision are rejected, so a
    /// delayed or stale acknowledgement can never move the caret back.
    /// The document serves line queries (RFC-0011 S4); `text` serves the
    /// `&str` selection APIs (user-paced).
    pub fn restore_interaction(
        &mut self,
        document: &DocumentText,
        text: &str,
        interaction: EditorInteractionSnapshot,
    ) {
        if self.buffer_id != Some(interaction.buffer_id)
            || self.revision != Some(interaction.revision)
            || interaction.sequence <= self.interaction_sequence
        {
            return;
        }
        // Restore the whole acknowledged set (X04) with its primary member
        // (E-4/FR-4); the primary pair stays authoritative for consumers
        // predating multi-member support.
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
        self.selections
            .replace_with(text, &members, interaction.primary);
        self.viewport.first_line =
            document.line_index_of(interaction.viewport_anchor.min(document.len()));
        self.focused = interaction.focused;
        self.ime_active = interaction.focused;
        self.interaction_sequence = interaction.sequence;
        // A reparented editor has not measured its viewport yet. Preserve
        // the host's scroll anchor instead of treating the default one-row
        // height as a reason to scroll the caret to the top.
        if self.focused && self.text_width > 0.0 {
            self.ensure_caret_visible(document);
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

    /// Materialized bytes for a view revision, cached per revision
    /// (RFC-0011 S3/S4). One O(n) materialization per revision, populated
    /// lazily: the structure bridge (below the plain gate) and the
    /// user-paced interaction paths (selection/word-bound/autocomplete APIs
    /// that take `&str`) use it; shaping, line geometry, and paint never
    /// do — they query the tree. Repeated calls for the same revision clone
    /// the `Arc` (O(1)).
    fn materialized_text(&self, view: &EditorView) -> std::sync::Arc<str> {
        if let Some((revision, text)) = self.materialized_text.borrow().as_ref()
            && *revision == view.revision
        {
            return text.clone();
        }
        let text = view.text.materialize();
        *self.materialized_text.borrow_mut() = Some((view.revision, text.clone()));
        text
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
            // RFC-0008 S1 (FR-10): the structure-parse phase (`render.scan`).
            // RFC-0011 S4: the full-document line-index build is gone; the
            // structure routing is plain-gate → pulldown bridge. Below the
            // gate the bridge runs over ONE materialized copy per revision
            // (transitional — the segment cache in S5 replaces this phase).
            let _scan_span =
                tracing::info_span!("render.scan", doc_len = view.text.len()).entered();
            if view.text.len() < HUGE_TEXT_THRESHOLD {
                // S5 removes the last O(n) keystroke cost.
                let text = self.materialized_text(view);
                let (link_spans, decorations) = structure_spans(&text);
                self.link_spans = link_spans;
                self.decorations = decorations;
            } else {
                // Plain mode: zero structure work, zero materialization.
                self.link_spans.clear();
                self.decorations.clear();
            }
            self.revision = Some(view.revision);
        }
        if refresh_resolutions && (!link_resolutions.is_empty() || self.resolutions_present) {
            for span in &mut self.link_spans {
                let key = flavite_syntax::text::fold_key(&span.payload.target_raw);
                span.resolution = resolve_link(span, link_resolutions.get(&key).copied());
            }
        }
        self.resolutions_present = !link_resolutions.is_empty();
        self.selections.clamp_document(&view.text);
        self.viewport.line_height = self.type_metrics.line_height;
        self.viewport.first_line = self
            .viewport
            .first_line
            .min(view.text.line_count().saturating_sub(1));
        // Only re-anchor the viewport when the caret moved. Running this
        // every frame would snap the view back to the caret and cancel any
        // wheel or scrollbar scroll away from it.
        if self.selections.caret() != self.last_ensured_caret {
            self.ensure_caret_visible(&view.text);
            self.last_ensured_caret = self.selections.caret();
        }
    }

    /// Keep the caret's line inside the viewport via tree line queries
    /// (O(log n) — no full-document scan). RFC-0011 S4.
    fn ensure_caret_visible(&mut self, document: &DocumentText) {
        let caret = self.selections.caret().min(document.len());
        let line = document.line_index_of(caret);
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

    /// Slice `lines` logical lines starting at `first` via tree line queries
    /// (O(log n + band)). RFC-0011 S4: the per-revision full-document line
    /// index is gone; band geometry is derived from the tree on demand.
    fn slice_lines(&self, document: &DocumentText, first: usize, lines: usize) -> (String, usize) {
        let line_count = document.line_count();
        let start_line = first.min(line_count.saturating_sub(1));
        let start = document.line_start_of(start_line);
        let end_line = start_line.saturating_add(lines.max(1)).min(line_count);
        let end = if end_line == line_count {
            document.len()
        } else {
            document.line_start_of(end_line)
        };
        (document.slice(start..end), start)
    }

    /// Byte offset of logical line `first_line` in the source (O(log n)).
    fn viewport_anchor(&self, document: &DocumentText) -> usize {
        let line = self
            .viewport
            .first_line
            .min(document.line_count().saturating_sub(1));
        document.line_start_of(line)
    }

    /// Pointer selection against shaped text: places or extends the
    /// selection at the glyph boundary under the point. Takes `&str` (a
    /// test/convenience seam — no interactive path calls it); the hit test
    /// runs against a tree built from the text, the selection moves use the
    /// contiguous text directly.
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
        let document = DocumentText::from(text);
        let byte = self.hit_test_cached(&document, revision, self.viewport, x, y, width);
        self.move_to(text, byte, extend);
    }

    /// Byte ranges of logical lines `[first, first + lines)` via tree line
    /// queries (O(lines × log n)). RFC-0011 S4.
    fn band_line_ranges(
        &self,
        document: &DocumentText,
        first: usize,
        lines: usize,
    ) -> Vec<(usize, usize)> {
        let line_count = document.line_count();
        let first = first.min(line_count.saturating_sub(1));
        let last = (first + lines).min(line_count);
        (first..last)
            .map(|index| {
                let start = document.line_start_of(index);
                let end = if index + 1 < line_count {
                    document.line_start_of(index + 1)
                } else {
                    document.len()
                };
                (start, end)
            })
            .collect()
    }

    /// The cached band's structural invariants (D10 self-heal gate): hashes,
    /// line starts, and buffer line count must all agree with the band.
    fn shaping_structurally_consistent(cache: &ShapingCacheV2) -> bool {
        let expected = expected_buffer_line_count(&cache.band.visible);
        cache.line_hashes.len() == expected
            && cache.line_starts.len() == expected
            && cache.buffer.lines.len() == expected
    }

    /// Full rebuild (fresh `Buffer`, discard per-line state): the D10
    /// self-heal / FR-2 fallback. Width/type/band-identity changes and
    /// structural inconsistencies land here.
    fn rebuild_shaping(
        &mut self,
        document: &DocumentText,
        revision: u64,
        target: (usize, usize),
        width: f32,
    ) {
        let (band_start, band_lines) = target;
        let (visible, source_start) = self.slice_lines(document, band_start, band_lines);
        let line_starts = shape_line_starts(&visible);
        let line_hashes = line_fingerprints(&visible, &line_starts);
        let buffer = shape_text(&visible, width, self.type_metrics);
        debug_assert_eq!(
            buffer.lines.len(),
            expected_buffer_line_count(&visible),
            "fresh band buffer line count"
        );
        #[cfg(test)]
        let buffer_lines = buffer.lines.len();
        self.shaping = Some(ShapingCacheV2 {
            buffer,
            band: BandState {
                band_start,
                band_lines,
                source_start,
                revision,
                visible,
            },
            width_bits: width.to_bits(),
            type_bits: shaping_type_bits(self.type_metrics),
            line_hashes,
            line_starts,
            #[cfg(test)]
            stats: SpliceStats {
                constructed: buffer_lines,
                removed: 0,
            },
        });
    }

    /// Splice the persistent buffer to match `band` (D2/D10): diff the band's
    /// desired `BufferLine` list against the current lines, replace/insert/
    /// remove only what changed, then one `shape_until_scroll`.
    fn splice_shaping(&mut self, document: &DocumentText, revision: u64, band: (usize, usize)) {
        let (band_start, band_lines) = band;
        let ranges = self.band_line_ranges(document, band_start, band_lines);

        // Cosmic-text's trailing-empty-line rule: an extra `BufferLine` exists
        // when the band text ends with a line ending (or the band is empty).
        let want_trailing_empty = ranges.last().is_some_and(|&(start, end)| {
            split_line_content_and_ending(&document.slice(start..end)).1
                != cosmic_text::LineEnding::None
        });
        let wanted_lines = ranges.len() + usize::from(want_trailing_empty);

        #[cfg(test)]
        let mut constructed = 0usize;
        #[cfg(test)]
        let mut removed = 0usize;
        let reshaped;
        {
            let cache = self.shaping.as_mut().expect("shaping cache present");
            let attrs = mono_attrs_list();
            let lines = &mut cache.buffer.lines;
            reshaped = {
                let mut reshaped = Vec::new();
                for (index, &(start, end)) in ranges.iter().enumerate() {
                    let piece = document.slice(start..end);
                    let (content, ending) = split_line_content_and_ending(&piece);
                    match lines.get_mut(index) {
                        Some(line) if line.text() != content => {
                            // Reuse the line allocation; marks shape/layout unused so
                            // the targeted reshape below re-shapes exactly this line.
                            line.reset_new(
                                content.to_owned(),
                                ending,
                                attrs.clone(),
                                Shaping::Advanced,
                            );
                            reshaped.push(index);
                            #[cfg(test)]
                            {
                                constructed += 1;
                            }
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                if lines.len() < wanted_lines {
                    let start = lines.len();
                    reshaped.extend(start..wanted_lines);
                    lines.extend((start..wanted_lines).map(|index| {
                        if let Some(&(range_start, range_end)) = ranges.get(index) {
                            let piece = document.slice(range_start..range_end);
                            let (content, ending) = split_line_content_and_ending(&piece);
                            BufferLine::new(
                                content.to_owned(),
                                ending,
                                attrs.clone(),
                                Shaping::Advanced,
                            )
                        } else {
                            // The trailing empty `BufferLine`.
                            BufferLine::new(
                                String::new(),
                                cosmic_text::LineEnding::None,
                                attrs.clone(),
                                Shaping::Advanced,
                            )
                        }
                    }));
                    #[cfg(test)]
                    {
                        constructed += lines.len() - start;
                    }
                } else if lines.len() > wanted_lines {
                    #[cfg(test)]
                    {
                        removed = lines.len() - wanted_lines;
                    }
                    lines.truncate(wanted_lines);
                }
                reshaped
            };
            // Hashes align with every `BufferLine` (logical lines + the
            // trailing empty line), parallel to `line_starts` and
            // `buffer.lines` (structural invariant in D10). RFC-0011 S4
            // (A9-class): only the changed/entering lines' hashes are
            // recomputed — untouched lines keep theirs (the diff loop above
            // guarantees a line's content equals the source line when it is
            // not in `reshaped`), so a keystroke costs O(changed), not
            // O(band).
            {
                let hashes = &mut cache.line_hashes;
                if hashes.len() < wanted_lines {
                    hashes.resize(wanted_lines, (0, 0));
                }
                for &index in &reshaped {
                    let hash = match ranges.get(index) {
                        Some(&(start, end)) => line_fingerprint(&document.slice(start..end)),
                        None => line_fingerprint(""),
                    };
                    hashes[index] = hash;
                }
                hashes.truncate(wanted_lines);
            }
        }

        let visible: String = ranges
            .iter()
            .map(|&(start, end)| document.slice(start..end))
            .collect();
        let line_starts = shape_line_starts(&visible);
        let cache = self.shaping.as_mut().expect("shaping cache present");
        cache.band.visible = visible;
        cache.line_starts = line_starts;
        cache.band.source_start = ranges.first().map_or(0, |&(start, _)| start);
        cache.band.band_start = band_start;
        cache.band.band_lines = band_lines;
        cache.band.revision = revision;
        #[cfg(test)]
        {
            cache.stats = SpliceStats {
                constructed,
                removed,
            };
        }

        // RFC-0008 S2: shape ONLY the changed/entering lines (the persistent
        // buffer's untouched lines keep their cached layout — D2). This is
        // the keystroke splice cost: O(changed lines), not O(band). The
        // parameters mirror `shape_text`'s `set_size`/`set_wrap` path
        // (WordOrGlyph, no mono width, tab 8).
        FONT_SYSTEM.with(|font_system| {
            let mut font_system = font_system.borrow_mut();
            let cache = self.shaping.as_mut().expect("shaping cache present");
            let metrics = cache.buffer.metrics();
            let width = f32::from_bits(cache.width_bits);
            for &index in &reshaped {
                if let Some(line) = cache.buffer.lines.get_mut(index) {
                    line.layout(
                        &mut font_system,
                        metrics.font_size,
                        Some(width),
                        cosmic_text::Wrap::WordOrGlyph,
                        None,
                        8,
                    );
                }
            }
        });
    }

    /// Read-only cache view for `draw` (no mutation allowed there): the
    /// persistent band when it still covers the viewport and agrees with the
    /// current revision/width/type and its own structural invariants.
    fn cached_shaped_view(
        &self,
        revision: u64,
        viewport: Viewport,
        width: f32,
    ) -> Option<ShapedView<'_>> {
        let cache = self.shaping.as_ref()?;
        if cache.width_bits != width.to_bits()
            || cache.type_bits != shaping_type_bits(self.type_metrics)
            || cache.band.revision != revision
            || !band_covers(cache.band.band_start, cache.band.band_lines, viewport)
            || !Self::shaping_structurally_consistent(cache)
        {
            return None;
        }
        Some(ShapedView {
            buffer: &cache.buffer,
            visible: cache.band.visible.as_str(),
            source_start: cache.band.source_start,
            band_start: cache.band.band_start,
            line_starts: &cache.line_starts,
        })
    }

    /// Sync the persistent shaping band to the current (document, revision,
    /// viewport, width) (RFC-0008 S2, RFC-0011 S4). A keystroke splices
    /// only the changed lines; scrolling splices band ends; full rebuilds
    /// happen only per FR-2/D10. Band text is sliced from the tree — no
    /// full-document materialization.
    fn sync_shaped(
        &mut self,
        document: &DocumentText,
        revision: u64,
        viewport: Viewport,
        width: f32,
    ) -> ShapedView<'_> {
        let line_count = document.line_count();
        let target = band_window(viewport, line_count);
        // RFC-0008 S1 (FR-10): the shaping phase. Plain integer fields.
        let _shaping_span = tracing::info_span!(
            "render.shaping",
            band_start = target.0,
            band_lines = target.1
        )
        .entered();

        enum Step {
            Rebuild,
            Splice,
            Fresh,
        }
        let covered = self.shaping.as_ref().is_some_and(|cache| {
            band_covers(cache.band.band_start, cache.band.band_lines, viewport)
        });
        let step = match self.shaping.as_ref() {
            None => Step::Rebuild,
            Some(cache) => {
                let invalid_geometry = cache.width_bits != width.to_bits()
                    || cache.type_bits != shaping_type_bits(self.type_metrics)
                    || !Self::shaping_structurally_consistent(cache);
                let band_must_move = !covered;
                let move_is_disjoint = band_must_move
                    && bands_disjoint(cache.band.band_start, cache.band.band_lines, target);
                if invalid_geometry || move_is_disjoint {
                    Step::Rebuild
                } else if band_must_move || cache.band.revision != revision {
                    Step::Splice
                } else {
                    Step::Fresh
                }
            }
        };

        match step {
            Step::Rebuild => self.rebuild_shaping(document, revision, target, width),
            Step::Splice => {
                let band = if covered {
                    let cache = self.shaping.as_ref().expect("shaping cache present");
                    (cache.band.band_start, cache.band.band_lines)
                } else {
                    target
                };
                self.splice_shaping(document, revision, band);
            }
            Step::Fresh => {}
        }

        let cache = self
            .shaping
            .as_ref()
            .expect("shaping cache was just populated");
        ShapedView {
            buffer: &cache.buffer,
            visible: cache.band.visible.as_str(),
            source_start: cache.band.source_start,
            band_start: cache.band.band_start,
            line_starts: &cache.line_starts,
        }
    }

    /// RFC-0008 S2-T4 harness: run one sync (rebuild or splice) headlessly.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn sync_shaped_for_perf(
        &mut self,
        document: &DocumentText,
        revision: u64,
        viewport: Viewport,
        width: f32,
    ) {
        let _ = self.sync_shaped(document, revision, viewport, width);
    }

    /// RFC-0008 S3-T5 harness: run the paint-collection path (spans +
    /// painted-line set + per-line content) headlessly and return the painted
    /// line count (all the work `draw` does before `fill_text`).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn painted_line_count_for_perf(
        &mut self,
        document: &DocumentText,
        revision: u64,
        viewport: Viewport,
        width: f32,
        paint_height: f32,
    ) -> usize {
        let line_height = self.type_metrics.line_height;
        let shaped = self.sync_shaped(document, revision, viewport, width);
        let spans = band_line_spans(shaped.buffer, shaped.line_starts.len(), line_height);
        let painted = painted_line_indices(&spans, shaped.paint_offset(viewport), paint_height);
        let mut count = 0usize;
        for index in painted {
            let _ = band_line_content(shaped.visible, shaped.line_starts, index);
            count += 1;
        }
        count
    }

    /// Hit-test against the cached band shaping. The point is viewport-local;
    /// the band's rows above the viewport are added back before the hit.
    fn hit_test_cached(
        &mut self,
        document: &DocumentText,
        revision: u64,
        viewport: Viewport,
        x: f32,
        y: f32,
        width: f32,
    ) -> usize {
        let view = self.sync_shaped(document, revision, viewport, width);
        let Some(cursor) = view
            .buffer
            .hit(x.max(0.0), y.max(0.0) + view.paint_offset(viewport))
        else {
            return view.source_start + view.visible.len();
        };
        let line_start = view.line_offset(cursor.line);
        view.source_start
            + crate::viewport::clamp_grapheme_boundary(view.visible, line_start + cursor.index)
                .min(view.visible.len())
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
        // RFC-0011 S4: the shaping band is sliced from the tree — layout
        // never materializes the document.
        state.sync_shaped(
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
        // RFC-0011 S4: shaping, line geometry, and paint query the tree; the
        // document is never materialized on the frame path. Interactive
        // overlays (context preview, autocomplete, composition) materialize
        // lazily below.
        let document = &self.view.text;
        // The persistent band covers the viewport plus its margin. When the
        // cache misses at draw time (the viewport moved between layout and
        // draw, or a width/type/revision change bypassed layout), shape
        // locally rather than paint a stale band.
        let local_buffer;
        let local_visible;
        let local_starts;
        let shaped_view: ShapedView<'_> = if let Some(view) =
            state.cached_shaped_view(self.view.revision, state.viewport, text_width)
        {
            view
        } else {
            let (band_start, band_lines) = band_window(state.viewport, document.line_count());
            let (visible, start) = state.slice_lines(document, band_start, band_lines);
            local_starts = shape_line_starts(&visible);
            local_buffer = shape_text(&visible, text_width, self.type_metrics);
            local_visible = visible;
            ShapedView {
                buffer: &local_buffer,
                visible: local_visible.as_str(),
                source_start: start,
                band_start,
                line_starts: &local_starts,
            }
        };
        // RFC-0008 S1 (FR-10): the paint phase (per-frame fill_text + quad
        // geometry). Emission happens on the real render path; the S3
        // headless-renderer double drives it in tests.
        let _paint_span = tracing::info_span!("render.paint").entered();
        let paint_offset = shaped_view.paint_offset(state.viewport);
        // Bound selection work by the physical rows being painted, even
        // when one logical line wraps far below the viewport. Band rows
        // above the viewport start sit at negative paint positions and are
        // culled here.
        let paint_height = (bounds.height - PAD_Y * 2.0).max(0.0);
        let runs: Vec<_> = shaped_view
            .buffer
            .layout_runs()
            .filter(|run| {
                let top = run.line_top - paint_offset;
                top < paint_height && top + self.type_metrics.line_height > 0.0
            })
            .collect();
        let source_start = shaped_view.source_start;
        let visible_text = shaped_view.visible;
        let line_starts = shaped_view.line_starts;
        let visible_end = source_start
            + runs
                .iter()
                .map(|run| {
                    let start = line_starts
                        .get(run.line_i)
                        .copied()
                        .unwrap_or(visible_text.len());
                    let end = run.glyphs.iter().map(|glyph| glyph.end).max().unwrap_or(0);
                    start + end
                })
                .max()
                .unwrap_or(0);
        let paint_text = &visible_text[..visible_end
            .saturating_sub(source_start)
            .min(visible_text.len())];
        let visible_members = paint_members(state.selections.members(), source_start, visible_end);
        // OFF-state muted fill (FEUX-0002 FR-13): a surviving multi-member
        // set renders with the muted channel while Context Mode is off; the
        // standing count in the note chrome is the non-color redundancy.
        let selection_fill = if !self.context_mode.active() && state.selections.len() > 1 {
            self.colors.selection_muted
        } else {
            self.colors.selection
        };
        if !visible_members.is_empty() {
            for run in &runs {
                let line_start = source_start
                    + line_starts
                        .get(run.line_i)
                        .copied()
                        .unwrap_or(visible_text.len());
                for glyph in run.glyphs {
                    let glyph_start = line_start + glyph.start;
                    let glyph_end = line_start + glyph.end;
                    let at = visible_members.partition_point(|member| member.end() <= glyph_start);
                    let hit = visible_members.get(at).is_some_and(|member| {
                        glyph_end > member.start() && glyph_start < member.end()
                    });
                    if hit {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: Rectangle::new(
                                    Point::new(
                                        bounds.x + PAD_X + glyph.x,
                                        bounds.y + PAD_Y + run.line_top - paint_offset,
                                    ),
                                    Size::new(glyph.w.max(1.0), self.type_metrics.line_height),
                                ),
                                border: Border::default(),
                                ..renderer::Quad::default()
                            },
                            Background::Color(selection_fill),
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
            // a destructive invocation (FEUX-0002 X04, E-6). The context
            // preview is a user-paced overlay — materialize on demand, never
            // on the typing path.
            let text = state.materialized_text(&self.view);
            for range in
                flavite_edit_context::effect::implicit_ranges(&text, visible_members, command)
            {
                for rect in command_preview_rects(
                    paint_text,
                    source_start,
                    line_starts,
                    &runs,
                    range,
                    self.type_metrics,
                    paint_offset,
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
        if let Some(spans) = &self.match_preview {
            // Match-selection draft overlay (FEUX-0002 X06/E-11): outline-
            // only, viewport-bounded, never a committed representation.
            for rect in match_preview_rects(
                paint_text,
                source_start,
                line_starts,
                &runs,
                spans,
                self.type_metrics,
                paint_offset,
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
                                width: 1.0,
                                ..Border::default()
                            },
                            ..renderer::Quad::default()
                        },
                        Background::Color(Color::TRANSPARENT),
                    );
                }
            }
        }
        // Use the semantic scanner for link decorations. No markdown parser or
        // second link grammar is embedded in the widget. RFC-0011 S4 (A8):
        // spans intersecting each line are picked by partition-point windows,
        // and glyphs and spans are both source-ordered, so a two-finger walk
        // advances through each line's active spans — O(glyphs + spans) per
        // line instead of O(glyphs × spans). The line end is band-relative
        // (the band always ends on a line boundary), so no full-document
        // `line_bounds` scan runs per line.
        for run in &runs {
            let line_start = source_start
                + line_starts
                    .get(run.line_i)
                    .copied()
                    .unwrap_or(visible_text.len());
            let line_end = source_start
                + line_starts
                    .get(run.line_i + 1)
                    .copied()
                    .unwrap_or(visible_text.len());
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
            let decisions = per_glyph_decorations(
                active_links,
                active_decorations,
                run.glyphs
                    .iter()
                    .map(|glyph| (line_start + glyph.start, line_start + glyph.end)),
            );
            for (glyph, (link_resolution, other_decoration)) in run.glyphs.iter().zip(decisions) {
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
                                    bounds.y + PAD_Y + run.line_top - paint_offset
                                        + self.type_metrics.line_height
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
        // RFC-0008 S3 (D3): paint one logical line per `fill_text` — iced's
        // paragraph cache is keyed by content+bounds, so unchanged lines are
        // cache hits and only the edited line's paragraph re-allocates; the
        // cache stays bounded by iced's per-frame trim. Whole-line
        // granularity (FR-3): the bottom edge rounds to whole lines.
        let spans = band_line_spans(
            shaped_view.buffer,
            line_starts.len(),
            self.type_metrics.line_height,
        );
        for index in painted_line_indices(&spans, paint_offset, paint_height) {
            let (top, height) = spans[index];
            renderer.fill_text(
                text::Text {
                    content: band_line_content(visible_text, line_starts, index),
                    bounds: Size::new(text_width, height.max(1.0)),
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
                Point::new(bounds.x + PAD_X, bounds.y + PAD_Y + top - paint_offset),
                self.colors.text,
                clip,
            );
        }
        // Scrollbar: only when the document overflows the viewport. The
        // wheel and gutter-drag share this geometry. Line count is the tree's
        // O(1) summary (RFC-0011 S4 A9: the byte-scan fallback is gone).
        let total_lines = document.line_count();
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
            && composition.start <= visible_end
        {
            let local_start = composition.start - source_start;
            let (x, y) =
                caret_position(&runs, visible_text, line_starts, local_start, paint_offset);
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
                let line = document.line_index_of(state.selections.caret().min(document.len()));
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
            for member in visible_members {
                if member.head() < source_start || member.head() > visible_end {
                    continue;
                }
                let local_caret = member.head().saturating_sub(source_start);
                let (x, y) =
                    caret_position(&runs, visible_text, line_starts, local_caret, paint_offset);
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
        // RFC-0011 S4: no eager materialization per event. Tree/band queries
        // serve hit-testing, line geometry, and paint; `&str`-based
        // interaction APIs (word bounds, selection ops, autocomplete)
        // materialize lazily in their own branches — user-paced, never on
        // the typing path.
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
                let text = state.materialized_text(&self.view);
                let (a, b) = flavite_edit_context::word_bounds(&text, byte);
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
                        // X02: Use inline motion check since widget doesn't have access to keymap
                        // Vertical motions: K=Up, J=Down (bare and shifted per default keymap)
                        match code {
                            flavite_edit_context::KeyCode::KeyK => {
                                Some(flavite_edit_context::Motion::Up)
                            }
                            flavite_edit_context::KeyCode::KeyJ => {
                                Some(flavite_edit_context::Motion::Down)
                            }
                            _ => None,
                        }
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
                self.advance_capture(state);
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
                    clipboard_acquired: false,
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
                    self.advance_capture(state);
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
                    let total_lines = self.view.text.line_count();
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
                        let text = state.materialized_text(&self.view);
                        state.select_click(
                            &text,
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
                    let total_lines = self.view.text.line_count();
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
                    let text = state.materialized_text(&self.view);
                    state.extend_pointer_selection(&text, byte);
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
                let total_lines = self.view.text.line_count();
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
                let completion_control = state.autocomplete_start.is_some()
                    && matches!(
                        key,
                        Key::Named(
                            Named::Enter | Named::ArrowUp | Named::ArrowDown | Named::Escape
                        )
                    )
                    && !modifiers.control()
                    && !modifiers.alt()
                    && !modifiers.logo();
                let escape_collapse = matches!(key, Key::Named(Named::Escape))
                    && !completion_control
                    && self.context_mode == flavite_edit_context::ContextMode::Off
                    && !modifiers.control()
                    && !modifiers.alt()
                    && !modifiers.logo()
                    && {
                        let text = state.materialized_text(&self.view);
                        state.selections.escape_collapse(&text)
                    };
                if (completion_control && self.autocomplete_key(state, key, shell))
                    || escape_collapse
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
            let viewport = state.viewport;
            let (x, y) = {
                let shaped =
                    state.sync_shaped(&self.view.text, self.view.revision, viewport, width);
                let runs = shaped.buffer.layout_runs().collect::<Vec<_>>();
                let paint_offset = shaped.paint_offset(viewport);
                let caret = caret_byte
                    .saturating_sub(shaped.source_start)
                    .min(shaped.visible.len());
                caret_position(
                    &runs,
                    shaped.visible,
                    shaped.line_starts,
                    caret,
                    paint_offset,
                )
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
                self.advance_capture(state);
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

/// Per-glyph link/decoration decisions for one shaped run (RFC-0011 S4
/// A8). `active_links`/`active_decorations` are the run's line window,
/// source-ordered and non-overlapping; glyphs arrive in source order. A
/// two-finger walk advances through each span list as glyphs pass span
/// ends — O(glyphs + spans), replacing the per-glyph `.find`/`.any` scans.
fn per_glyph_decorations(
    active_links: &[LinkSpan],
    active_decorations: &[flavite_syntax::ScannedSpan],
    glyphs: impl Iterator<Item = (usize, usize)>,
) -> Vec<(Option<LinkResolution>, bool)> {
    let mut out = Vec::new();
    let mut link_pos = 0usize;
    let mut decoration_pos = 0usize;
    for (glyph_start, glyph_end) in glyphs {
        while link_pos < active_links.len() && active_links[link_pos].span.end <= glyph_start {
            link_pos += 1;
        }
        let link_resolution = active_links
            .get(link_pos)
            .filter(|span| span.span.start < glyph_end)
            .map(|span| span.resolution);
        while decoration_pos < active_decorations.len()
            && active_decorations[decoration_pos].span.end.0 <= glyph_start
        {
            decoration_pos += 1;
        }
        let other_decoration = active_decorations
            .get(decoration_pos)
            .is_some_and(|span| span.span.start.0 < glyph_end);
        out.push((link_resolution, other_decoration));
    }
    out
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

/// Shape and hit-test a logical-line slice.
/// Test-only reference implementation; the widget path goes through the
/// cached shaping in [`EditorState::shaped`].
#[cfg(test)]
fn hit_test_visible(
    text: &str,
    first_line: usize,
    lines: usize,
    x: f32,
    y: f32,
    width: f32,
) -> usize {
    let (visible, source_start) = slice_lines(text, first_line, lines);
    let buffer = shape_text(&visible, width, TypeMetrics::default());
    let Some(cursor) = buffer.hit(x.max(0.0), y.max(0.0)) else {
        return source_start + visible.len();
    };
    let line_start = line_start_byte(&visible, cursor.line);
    source_start + clamp_char_boundary(&visible, line_start + cursor.index).min(visible.len())
}

/// Slice `lines` logical lines starting at `first` (byte-scan reference;
/// the widget path uses tree queries in [`EditorState::slice_lines`]).
#[cfg(test)]
fn slice_lines(text: &str, first: usize, lines: usize) -> (String, usize) {
    let line_count = logical_line_count(text);
    let start_line = first.min(line_count.saturating_sub(1));
    let start = line_start_byte(text, start_line);
    let end_line = start_line.saturating_add(lines.max(1)).min(line_count);
    let end = if end_line == line_count {
        text.len()
    } else {
        line_start_byte(text, end_line)
    };
    (text[start..end].to_owned(), start)
}

#[cfg(test)]
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

#[cfg(test)]
fn line_start_byte(text: &str, line: usize) -> usize {
    text.split_inclusive('\n').take(line).map(str::len).sum()
}

/// Geometry is derived from the exact shaped glyph clusters used for text.
/// Joining only visually adjacent rectangles avoids boxing unrelated text in
/// mixed-direction rows. Only the shaped viewport is inspected.
fn command_preview_rects(
    visible: &str,
    source_start: usize,
    line_starts: &[usize],
    runs: &[cosmic_text::LayoutRun<'_>],
    range: std::ops::Range<usize>,
    metrics: TypeMetrics,
    paint_offset: f32,
) -> Vec<Rectangle> {
    let mut result = Vec::new();
    for run in runs {
        let line_start = source_start
            + line_starts
                .get(run.line_i)
                .copied()
                .unwrap_or(visible.len());
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
                        Point::new(start, run.line_top - paint_offset),
                        Size::new(end - start, metrics.line_height),
                    ));
                }
                merged = Some((left, right));
            }
        }
        if let Some((start, end)) = merged {
            result.push(Rectangle::new(
                Point::new(start, run.line_top - paint_offset),
                Size::new(end - start, metrics.line_height),
            ));
        }
    }
    // Newlines have no glyph. An outlined cell makes an empty-line Cut visible
    // and distinguishes copying/cutting a line with its terminator from Delete.
    for (local, ch) in visible.char_indices() {
        if ch == '\n' && range.contains(&(source_start + local)) {
            let (x, y) = caret_position(runs, visible, line_starts, local, paint_offset);
            result.push(Rectangle::new(
                Point::new(x, y),
                Size::new(metrics.font_size * 0.6, metrics.line_height),
            ));
        }
    }
    result
}

/// Outline rects for a match-selection draft overlay (FEUX-0002 E-11).
/// `spans` must be sorted ascending; only spans intersecting the visible
/// source window cost work (partition-point per run, then per-glyph tests),
/// so paint scales with the viewport, never with N.
fn match_preview_rects(
    visible: &str,
    source_start: usize,
    line_starts: &[usize],
    runs: &[cosmic_text::LayoutRun<'_>],
    spans: &[(usize, usize)],
    metrics: TypeMetrics,
    paint_offset: f32,
) -> Vec<Rectangle> {
    let mut result = Vec::new();
    for run in runs {
        let line_start = source_start
            + line_starts
                .get(run.line_i)
                .copied()
                .unwrap_or(visible.len());
        let line_end = line_start + run.text.len();
        let first = spans.partition_point(|&(_, end)| end <= line_start);
        let last = spans.partition_point(|&(start, _)| start < line_end);
        let active = &spans[first..last];
        if active.is_empty() {
            continue;
        }
        let mut covered: Vec<(f32, f32)> = Vec::new();
        for glyph in run.glyphs {
            let glyph_start = line_start + glyph.start;
            let glyph_end = line_start + glyph.end;
            if active
                .iter()
                .any(|&(start, end)| glyph_end > start && glyph_start < end)
            {
                covered.push((glyph.x, glyph.x + glyph.w.max(1.0)));
            }
        }
        if covered.is_empty() {
            continue;
        }
        covered.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
        let mut merged: Option<(f32, f32)> = None;
        for (left, right) in covered {
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
                Point::new(start, run.line_top - paint_offset),
                Size::new(end - start, metrics.line_height),
            ));
        }
    }
    // Newlines have no glyph; a covered newline still gets a visible cell.
    for (local, ch) in visible.char_indices() {
        if ch == '\n' {
            let absolute = source_start + local;
            if spans
                .iter()
                .any(|&(start, end)| absolute >= start && absolute < end)
            {
                let (x, y) = caret_position(runs, visible, line_starts, local, paint_offset);
                result.push(Rectangle::new(
                    Point::new(x, y),
                    Size::new(metrics.font_size * 0.6, metrics.line_height),
                ));
            }
        }
    }
    result
}

/// The canonical intervals have monotonically ordered starts and ends.
/// Two binary searches select the paint window without cloning or scanning
/// offscreen members. Boundary carets are included for caret painting.
fn paint_members(
    members: &[flavite_buffer::selection::Selection],
    start: usize,
    end: usize,
) -> &[flavite_buffer::selection::Selection] {
    let first = members.partition_point(|member| member.end() < start);
    let last = members.partition_point(|member| member.start() <= end);
    &members[first..last]
}

pub(crate) fn caret_position(
    runs: &[cosmic_text::LayoutRun<'_>],
    text: &str,
    line_starts: &[usize],
    byte: usize,
    paint_offset: f32,
) -> (f32, f32) {
    for (index, run) in runs.iter().enumerate() {
        let line_start = line_starts.get(run.line_i).copied().unwrap_or(text.len());
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
                return (glyph.x, run.line_top - paint_offset);
            }
            if byte < glyph_end {
                return (glyph.x, run.line_top - paint_offset);
            }
        }
        if byte <= line_end {
            let x = run.glyphs.last().map_or(0.0, |glyph| glyph.x + glyph.w);
            return (x, run.line_top - paint_offset);
        }
    }
    (
        0.0,
        runs.last().map_or(0.0, |run| run.line_top) - paint_offset,
    )
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
        let starts = shape_line_starts(text);
        let rects = command_preview_rects(text, 100, &starts, &runs, 102..104, metrics, 0.0);
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
        let blank_line = command_preview_rects(text, 100, &starts, &runs, 107..108, metrics, 0.0);
        assert_eq!(
            blank_line.len(),
            1,
            "an empty line's newline has a visible cell"
        );
        assert!(blank_line[0].width > 0.0);
        assert!(
            command_preview_rects(text, 100, &starts, &runs, 0..50, metrics, 0.0).is_empty(),
            "offscreen source does not create paint primitives"
        );

        let wrapped = shape_text("one two three four five", 50.0, metrics);
        let runs = wrapped.layout_runs().collect::<Vec<_>>();
        let rects = command_preview_rects(
            "one two three four five",
            0,
            &shape_line_starts("one two three four five"),
            &runs,
            0..23,
            metrics,
            0.0,
        );
        assert!(
            rects.iter().any(|rect| rect.y > 0.0),
            "a wrapped line's outline follows its actual rows"
        );
    }

    #[test]
    fn paint_window_borrows_only_visible_members_at_every_cardinality() {
        let source = "abcdefghi\n".repeat(10_000);
        let metrics = TypeMetrics::default();
        let visible = &source[..49];
        let shaped = shape_text(visible, 300.0, metrics);
        let runs = shaped.layout_runs().collect::<Vec<_>>();
        let mut observed = None;
        for count in [100, 1_000, 10_000] {
            let raw = (0..count)
                .map(|index| flavite_buffer::selection::RawSelection {
                    id: Some(index as u32),
                    anchor: index * 10,
                    head: index * 10,
                })
                .collect();
            let (set, _) =
                flavite_buffer::selection::SelectionSet::normalize(&source, raw, Some(0)).unwrap();
            let members = paint_members(set.members(), 0, 49);
            assert_eq!(members.len(), 5);
            assert!(
                std::ptr::eq(members.as_ptr(), set.members().as_ptr()),
                "paint borrows rather than clones the set"
            );
            let ranges = flavite_edit_context::effect::implicit_ranges(
                &source,
                members,
                flavite_edit_context::ContextCommand::Cut,
            );
            let starts = shape_line_starts(visible);
            let primitives: usize = ranges
                .into_iter()
                .map(|range| {
                    command_preview_rects(visible, 0, &starts, &runs, range, metrics, 0.0).len()
                })
                .sum();
            assert!(primitives > 0);
            if let Some(previous) = observed {
                assert_eq!(
                    primitives, previous,
                    "offscreen selections add no primitives"
                );
            }
            observed = Some(primitives);
        }
    }

    #[test]
    fn match_preview_outlines_cover_their_spans_and_stay_viewport_bounded() {
        let text = "TODO one\nTODO two\n";
        let metrics = TypeMetrics::default();
        let shaped = shape_text(text, 300.0, metrics);
        let runs = shaped.layout_runs().collect::<Vec<_>>();
        let starts = shape_line_starts(text);
        let spans = [(0, 4), (9, 13)];
        let rects = match_preview_rects(text, 0, &starts, &runs, &spans, metrics, 0.0);
        assert!(!rects.is_empty());
        // Every glyph inside a span has a covering outline; glyphs outside
        // spans never do.
        for run in &runs {
            let line_start = line_start_byte(text, run.line_i);
            for glyph in run.glyphs {
                let center = Point::new(
                    glyph.x + glyph.w / 2.0,
                    run.line_top + metrics.line_height / 2.0,
                );
                let inside = spans.iter().any(|&(start, end)| {
                    line_start + glyph.end > start && line_start + glyph.start < end
                });
                assert_eq!(
                    rects.iter().any(|rect| rect.contains(center)),
                    inside,
                    "glyph at {}",
                    line_start + glyph.start
                );
            }
        }
        // Off-screen spans produce no primitives.
        assert!(match_preview_rects(text, 100, &starts, &runs, &[(0, 4)], metrics, 0.0).is_empty());
        // An empty list never paints.
        assert!(match_preview_rects(text, 0, &starts, &runs, &[], metrics, 0.0).is_empty());
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
            primary: 0,
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
        let text = view.text.materialize();
        let caret = text.find("line 8").unwrap();
        state.restore_interaction(
            &view.text,
            &text,
            EditorInteractionSnapshot {
                sequence: 1,
                buffer_id: view.buffer_id,
                revision: view.revision,
                caret,
                selection: (caret, caret),
                selections: vec![[0, caret, caret]],
                primary: 0,
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
            text: flavite_buffer::chunks::DocumentText::from(text.as_str()),
            path: std::path::PathBuf::from("long.md"),
            state: BufferState::Clean,
            last_edit: None,
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
        let text = view.text.materialize();
        state.selections.replace_with(&text, &[(far, far)], 0);
        state.reconcile(&view, &HashMap::new());
        assert!(state.viewport.first_line >= 140);
        assert_eq!(state.last_ensured_caret, state.selections.caret());
    }

    #[test]
    fn indexed_slices_match_source_slices_after_reload_and_empty_lines() {
        let mut state = EditorState::default();
        for (revision, text) in ["", "é\n\n🧑🏽‍💻\n", "no newline", "a\rb\nc\r\n"]
            .into_iter()
            .enumerate()
        {
            let mut view = long_view(0);
            view.revision = revision as u64;
            view.text = flavite_buffer::chunks::DocumentText::from(text);
            state.reconcile(&view, &HashMap::new());
            assert_eq!(view.text.line_count(), logical_line_count(text));
            for first_line in [0, 1, 3, 999] {
                for lines in [0, 1, 3] {
                    let viewport = Viewport {
                        first_line,
                        visible_lines: lines,
                        ..Viewport::default()
                    };
                    assert_eq!(
                        state.slice_lines(&view.text, first_line, lines),
                        slice_lines(text, first_line, lines),
                        "{first_line}+{lines} of {text:?}"
                    );
                    // The band always covers the clamped viewport start.
                    let count = view.text.line_count();
                    let (band_start, band_lines) = band_window(viewport, count);
                    let first = viewport.first_line.min(count.saturating_sub(1));
                    assert!(band_start <= first && first < band_start + band_lines);
                    assert!(band_lines >= 1);
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
    fn persistent_shaping_reuses_the_buffer_and_moves_the_band_by_splice() {
        let text = "one\ntwo\nthree\nfour\nfive";
        let viewport = Viewport {
            first_line: 2,
            visible_lines: 1,
            ..Viewport::default()
        };
        let mut state = EditorState::default();
        let shaped = state.sync_shaped(&DocumentText::from(text), 1, viewport, 300.0);
        let buffer_ptr = shaped.buffer as *const ShapingBuffer;
        // The band spans one viewport of margin around the viewport.
        assert_eq!(shaped.band_start, 1);
        assert_eq!(shaped.visible, "two\nthree\nfour\n");
        assert_eq!(shaped.source_start, text.find("two").unwrap());
        // Same revision + band: fresh path — identical buffer object.
        let again = state.sync_shaped(&DocumentText::from(text), 1, viewport, 300.0);
        assert!(std::ptr::eq(buffer_ptr, again.buffer));
        // Wheeling inside the band reuses the cached buffer; line offsets
        // resolve through the band's line starts.
        let scrolled = Viewport {
            first_line: 3,
            ..viewport
        };
        let scrolled_view = state.sync_shaped(&DocumentText::from(text), 1, scrolled, 300.0);
        assert!(std::ptr::eq(buffer_ptr, scrolled_view.buffer));
        assert_eq!(
            scrolled_view.source_start + scrolled_view.line_offset(2),
            text.find("four").unwrap()
        );
        // Leaving the band moves it by splice — the SAME buffer object persists
        // (only its lines change).
        let far = Viewport {
            first_line: 0,
            ..viewport
        };
        let moved = state.sync_shaped(&DocumentText::from(text), 1, far, 300.0);
        assert!(std::ptr::eq(buffer_ptr, moved.buffer));
        assert_eq!(moved.band_start, 0);
        assert_eq!(moved.visible, "one\ntwo\n");
        // A different revision (keystroke) splices in place — same buffer.
        let swapped = "axe\nbee\nceded\nfall\ngate";
        assert_eq!(swapped.len(), text.len());
        let revised = state.sync_shaped(&DocumentText::from(swapped), 2, viewport, 300.0);
        assert!(std::ptr::eq(buffer_ptr, revised.buffer));
        assert_eq!(revised.visible, "bee\nceded\nfall\n");
        // A different width rebuilds (new buffer).
        state.sync_shaped(&DocumentText::from(swapped), 2, viewport, 301.0);
        assert_eq!(
            state.shaping.as_ref().expect("cached").width_bits,
            301.0_f32.to_bits()
        );
    }

    #[test]
    fn splice_replaces_exactly_one_line_on_a_single_char_insert() {
        // S2-T1: a mid-band single-character insert splices exactly one
        // `BufferLine`; every other line keeps its text (and therefore its
        // shaped cache).
        let text = "alpha\nbeta\ngamma\ndelta\n";
        let viewport = Viewport {
            first_line: 1,
            visible_lines: 1,
            ..Viewport::default()
        };
        let mut state = EditorState::default();
        let first = state.sync_shaped(&DocumentText::from(text), 1, viewport, 300.0);
        let before: Vec<String> = first
            .buffer
            .lines
            .iter()
            .map(|line| line.text().to_owned())
            .collect();
        let edited = "alpha\nbeXta\ngamma\ndelta\n";
        let second = state.sync_shaped(&DocumentText::from(edited), 2, viewport, 300.0);
        let after: Vec<String> = second
            .buffer
            .lines
            .iter()
            .map(|line| line.text().to_owned())
            .collect();
        assert_eq!(before.len(), after.len());
        let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        assert_eq!(changed, 1, "exactly one BufferLine replaced");
        assert_eq!(after[1], "beXta");
        assert_eq!(second.visible, "alpha\nbeXta\ngamma\n");
    }

    #[test]
    fn band_travel_constructs_entering_lines_once_and_removes_exiting_lines() {
        // S2-T3: band travel splices band ends — entering lines are
        // constructed, exiting lines are removed, and re-entry reconstructs.
        let text = "a\nb\nc\nd\ne\nf\n";
        // Top band [0,6) (full doc): 6 content lines + trailing empty.
        let top = Viewport {
            first_line: 0,
            visible_lines: 3,
            ..Viewport::default()
        };
        // Bottom band clamps to [2,6): 4 content lines + trailing empty.
        let bottom = Viewport {
            first_line: 5,
            visible_lines: 3,
            ..Viewport::default()
        };
        let mut state = EditorState::default();
        {
            let s = state.sync_shaped(&DocumentText::from(text), 1, top, 300.0);
            assert_eq!(s.band_start, 0);
            assert_eq!(s.visible, "a\nb\nc\nd\ne\nf\n");
        }
        {
            let s = state.sync_shaped(&DocumentText::from(text), 1, bottom, 300.0);
            assert_eq!(s.band_start, 2);
            assert_eq!(s.visible, "c\nd\ne\nf\n");
        }
        let stats = &state.shaping.as_ref().expect("cache").stats;
        assert!(stats.removed >= 2, "exiting lines removed: {stats:?}");
        // Re-entering the top band constructs the entering lines again.
        {
            let s = state.sync_shaped(&DocumentText::from(text), 1, top, 300.0);
            assert_eq!(s.band_start, 0);
            assert_eq!(s.visible, "a\nb\nc\nd\ne\nf\n");
        }
        let stats = &state.shaping.as_ref().expect("cache").stats;
        assert!(
            stats.constructed >= 2,
            "entering lines constructed: {stats:?}"
        );
    }

    #[test]
    fn self_heal_rebuilds_when_the_cache_is_structurally_inconsistent() {
        // S2-T5 (D10): any structural inconsistency (hashes/line starts/
        // buffer line count) rebuilds from scratch on the next sync, and the
        // layout stays correct.
        let text = "one\ntwo\nthree\nfour\nfive\n";
        let viewport = Viewport {
            first_line: 1,
            visible_lines: 1,
            ..Viewport::default()
        };
        let mut state = EditorState::default();
        let shaped = state.sync_shaped(&DocumentText::from(text), 1, viewport, 300.0);
        let expected_visible = shaped.visible.to_owned();
        // Corrupt a structural invariant: drop one hash entry.
        state.shaping.as_mut().expect("cache").line_hashes.pop();
        let healed = state.sync_shaped(&DocumentText::from(text), 1, viewport, 300.0);
        assert_eq!(
            healed.visible, expected_visible,
            "self-heal restores the band"
        );
        let cache = state.shaping.as_ref().expect("cache");
        let expected = expected_buffer_line_count(&cache.band.visible);
        assert_eq!(cache.line_hashes.len(), expected);
        assert_eq!(cache.buffer.lines.len(), expected);
        // The healed cache is structurally consistent again.
        assert!(EditorState::shaping_structurally_consistent(cache));
    }

    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum TestOp {
        Insert { at: usize, ch: char },
        Delete { at: usize, len: usize },
        InsertNewline { at: usize },
        Scroll { first_line: usize },
        Resize { width: f32 },
    }

    fn floor_char_boundary(text: &str, mut at: usize) -> usize {
        while at > 0 && !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn apply_test_op(text: &mut String, op: &TestOp) {
        match op {
            TestOp::Insert { at, ch } => {
                let at = floor_char_boundary(text, (*at).min(text.len()));
                text.insert(at, *ch);
            }
            TestOp::InsertNewline { at } => {
                let at = floor_char_boundary(text, (*at).min(text.len()));
                text.insert(at, '\n');
            }
            TestOp::Delete { at, len } => {
                let at = floor_char_boundary(text, (*at).min(text.len()));
                let mut end = (at + *len).min(text.len());
                while end < text.len() && !text.is_char_boundary(end) {
                    end += 1;
                }
                text.drain(at..end);
            }
            TestOp::Scroll { .. } | TestOp::Resize { .. } => {}
        }
    }

    fn markdownish_base() -> impl Strategy<Value = String> {
        let line = prop_oneof![
            "[A-Za-z]{0,24}",
            Just("[[tag]]".to_owned()),
            Just("é漢😀".to_owned()),
            Just("x y z".to_owned()),
            Just("".to_owned()),
        ];
        prop::collection::vec(line, 3..14)
            .prop_map(|lines| lines.join("\n"))
            .prop_flat_map(|text| {
                let with_ending = format!("{text}\n");
                prop::bool::ANY.prop_map(move |newline| {
                    if newline {
                        with_ending.clone()
                    } else {
                        text.clone()
                    }
                })
            })
    }

    fn op_strategy() -> impl Strategy<Value = TestOp> {
        prop_oneof![
            (0..2048usize, prop::char::range('a', 'z'))
                .prop_map(|(at, ch)| TestOp::Insert { at, ch }),
            (0..2048usize, 0..64usize).prop_map(|(at, len)| TestOp::Delete { at, len }),
            (0..2048usize).prop_map(|at| TestOp::InsertNewline { at }),
            (0..512usize).prop_map(|first_line| TestOp::Scroll { first_line }),
            (60.0_f32..1400.0).prop_map(|width| TestOp::Resize { width }),
        ]
    }

    /// A lossless layout-run signature for the persistent-vs-fresh
    /// equivalence comparison (S2-T2).
    type RunSignature = (usize, usize, f32, f32, f32, Vec<(f32, f32, usize, usize)>);

    fn run_signature(run: cosmic_text::LayoutRun<'_>) -> RunSignature {
        (
            run.line_i,
            run.glyphs.len(),
            run.line_top,
            run.line_y,
            run.line_w,
            run.glyphs
                .iter()
                .map(|glyph| (glyph.x, glyph.w, glyph.start, glyph.end))
                .collect(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 40, ..ProptestConfig::default() })]

        /// S2-T2 (R1): the persistent buffer's layout equals a freshly built
        /// buffer for the same state after randomized edits, scrolls, and
        /// resizes — at every step.
        #[test]
        fn persistent_vs_fresh_layout_equivalence(
            base in markdownish_base(),
            ops in prop::collection::vec(op_strategy(), 8..48),
        ) {
            let mut text = base;
            let mut state = EditorState::default();
            let mut revision = 1u64;
            let mut viewport = Viewport {
                first_line: 0,
                visible_lines: 3,
                ..Viewport::default()
            };
            let mut width = 300.0_f32;
            for op in &ops {
                match op {
                    TestOp::Scroll { first_line } => viewport.first_line = *first_line,
                    TestOp::Resize { width: new_width } => width = *new_width,
                    _ => apply_test_op(&mut text, op),
                }
                revision += 1;
                let view = EditorView::from(&test_view(&text, 1));
                state.reconcile(&view, &HashMap::new());
                let shaped = state.sync_shaped(&DocumentText::from(text.as_str()), revision, viewport, width);
                let fresh = shape_text(shaped.visible, width, TypeMetrics::default());
                let runs_persistent: Vec<_> = shaped
                    .buffer
                    .layout_runs()
                    .map(run_signature)
                    .collect();
                let runs_fresh: Vec<_> = fresh.layout_runs().map(run_signature).collect();
                assert_eq!(
                    runs_persistent,
                    runs_fresh,
                    "layout divergence at revision {revision} (op {op:?}, band {}-{})",
                    shaped.band_start,
                    shaped.band_start + (shaped.line_starts.len()),
                );
                let lines_persistent: Vec<_> = shaped
                    .buffer
                    .lines
                    .iter()
                    .map(cosmic_text::BufferLine::text)
                    .collect();
                let lines_fresh: Vec<_> = fresh.lines.iter().map(cosmic_text::BufferLine::text).collect();
                assert_eq!(lines_persistent, lines_fresh, "line texts diverge");
            }
        }
    }

    /// Legacy whole-band paint oracle (S3-T1/T3): the pre-S3 mid-line slice's
    /// painted logical lines — lines whose start falls inside the old
    /// `paint_text` slice (kept only as the test oracle).
    fn legacy_painted_lines(
        visible: &str,
        starts: &[usize],
        source_start: usize,
        runs: &[cosmic_text::LayoutRun<'_>],
    ) -> Vec<usize> {
        let visible_end = source_start
            + runs
                .iter()
                .map(|run| {
                    let start = starts.get(run.line_i).copied().unwrap_or(visible.len());
                    let end = run.glyphs.iter().map(|glyph| glyph.end).max().unwrap_or(0);
                    start + end
                })
                .max()
                .unwrap_or(0);
        let paint_len = visible_end.saturating_sub(source_start).min(visible.len());
        starts
            .iter()
            .enumerate()
            .filter(|&(_, &start)| start < paint_len)
            .map(|(index, _)| index)
            .collect()
    }

    /// The draw path's visible-run filter (mirror of `draw`).
    fn visible_runs<'a>(
        buffer: &'a ShapingBuffer,
        paint_offset: f32,
        paint_height: f32,
        line_height: f32,
    ) -> Vec<cosmic_text::LayoutRun<'a>> {
        buffer
            .layout_runs()
            .filter(|run| {
                let top = run.line_top - paint_offset;
                top < paint_height && top + line_height > 0.0
            })
            .collect()
    }

    /// S3-T1 + S3-T3: the per-line painted set equals the legacy band-slice
    /// painted set up to one trailing partial line — never an earlier line
    /// (FR-3/R9).
    fn assert_painted_matches_legacy_within_trailing_line(
        state: &mut EditorState,
        text: &str,
        revision: u64,
        viewport: Viewport,
        width: f32,
        paint_height: f32,
        line_height: f32,
    ) {
        let doc = DocumentText::from(text);
        let shaped = state.sync_shaped(&doc, revision, viewport, width);
        let paint_offset = shaped.paint_offset(viewport);
        let legacy_runs = visible_runs(shaped.buffer, paint_offset, paint_height, line_height);
        let spans = band_line_spans(shaped.buffer, shaped.line_starts.len(), line_height);
        // The legacy full-band paragraph included (clipped) lines fully above
        // the viewport; the per-line paint does not paint them. For an
        // apples-to-apples comparison, restrict the legacy set to the lines
        // intersecting the paint window (the visible ones).
        let legacy = legacy_painted_lines(
            shaped.visible,
            shaped.line_starts,
            shaped.source_start,
            &legacy_runs,
        )
        .into_iter()
        .filter(|&index| {
            let (top, height) = spans[index];
            let top_viewport = top - paint_offset;
            top_viewport < paint_height && top_viewport + height > 0.0
        })
        .collect::<Vec<_>>();
        let painted = painted_line_indices(&spans, paint_offset, paint_height);
        // Difference is allowed only at the window edges (top/bottom partial
        // lines): strip the common interior, then at most one extra line may
        // remain at either end — a mid-list divergence (an earlier line
        // painted differently) leaves more, and fails.
        let mut a = painted.clone();
        let mut b = legacy.clone();
        while a.first() == b.first() && !a.is_empty() {
            a.remove(0);
            b.remove(0);
        }
        while a.last() == b.last() && !a.is_empty() {
            a.pop();
            b.pop();
        }
        assert!(
            a.len() <= 1 && b.len() <= 1,
            "painted {painted:?} vs legacy {legacy:?} (interior mismatch {a:?}/{b:?})"
        );
        assert!(painted.iter().all(|&i| i < shaped.line_starts.len()));
    }

    #[test]
    fn painted_lines_match_the_legacy_slice_within_whole_line_granularity() {
        for (text, first_line, visible, ratio) in [
            ("one\ntwo\nthree\nfour\nfive", 2usize, 1usize, 1.0f32),
            ("one\ntwo\nthree\nfour\nfive", 0, 3, 2.0),
            ("a\n\nb\n\nc\n\nd", 1, 2, 0.7),
            ("line\n".repeat(40).as_str(), 17, 5, 1.3),
            ("雪é漢😀\n".repeat(12).as_str(), 3, 4, 1.0),
            ("", 0, 1, 1.0),
        ] {
            let viewport = Viewport {
                first_line,
                visible_lines: visible,
                ..Viewport::default()
            };
            let line_height = TypeMetrics::default().line_height;
            let paint_height = visible as f32 * line_height * ratio;
            let mut state = EditorState::default();
            assert_painted_matches_legacy_within_trailing_line(
                &mut state,
                text,
                1,
                viewport,
                300.0,
                paint_height,
                line_height,
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 25, ..ProptestConfig::default() })]

        /// S3-T3 (R9): randomized viewports/heights never diverge on an
        /// earlier line — only the trailing partial line may differ.
        #[test]
        fn painted_content_differs_from_legacy_only_by_a_trailing_partial_line(
            base in markdownish_base(),
            first_line in 0usize..64,
            visible in 1usize..12,
            height_ratio in 0.4f32..20.0,
        ) {
            let viewport = Viewport {
                first_line,
                visible_lines: visible,
                ..Viewport::default()
            };
            let line_height = TypeMetrics::default().line_height;
            let paint_height = visible as f32 * line_height * height_ratio;
            let mut state = EditorState::default();
            assert_painted_matches_legacy_within_trailing_line(
                &mut state, &base, 1, viewport, 300.0, paint_height, line_height,
            );
        }
    }

    #[test]
    fn per_line_paragraphs_wrap_identically_to_the_band() {
        // S3-T2: a wrapped line painted standalone wraps to the same visual
        // rows the band layout reports for that line.
        let text = format!("{}\nshort\n", "word ".repeat(60));
        let width = 120.0_f32;
        let viewport = Viewport {
            first_line: 0,
            visible_lines: 2,
            ..Viewport::default()
        };
        let mut state = EditorState::default();
        let shaped = state.sync_shaped(&DocumentText::from(text.as_str()), 1, viewport, width);
        let line_index = 0usize;
        let line_content = band_line_content(shaped.visible, shaped.line_starts, line_index);
        let standalone = shape_text(&line_content, width, TypeMetrics::default());
        let band_rows: Vec<_> = shaped
            .buffer
            .layout_runs()
            .filter(|run| run.line_i == line_index)
            .map(|run| run_signature(run))
            .collect();
        let standalone_rows: Vec<_> = standalone.layout_runs().map(run_signature).collect();
        assert!(
            band_rows.len() >= 3,
            "the long line wraps into several rows (got {})",
            band_rows.len()
        );
        assert_eq!(standalone_rows, band_rows, "per-line wrap equals band wrap");
        // Selection across the wrapped line: the caret hit-testing already
        // resolves through ShapedView; assert the rows map to the same
        // logical line (geometry identity).
        assert!(band_rows.iter().all(|(line, ..)| *line == line_index));
    }

    #[test]
    fn headless_paint_double_bounds_paragraph_keys_across_keystrokes() {
        // S3-T4 (R2): with per-line granularity and iced's per-frame trim,
        // the paint double's paragraph keys stay bounded by the band. Run
        // 200 keystrokes (one mid-band char edit each) and track the live
        // key set under per-frame LRU trim (mirrors iced `Cache::trim`).
        let base = (0..200u32)
            .map(|index| format!("line {index:03} alpha beta gamma delta epsilon zeta eta theta\n"))
            .collect::<String>();
        let width = 800.0_f32;
        let viewport = Viewport {
            first_line: 50,
            visible_lines: 15,
            ..Viewport::default()
        };
        let line_height = TypeMetrics::default().line_height;
        let paint_height = 15.0 * line_height;
        let base_view = EditorView::from(&test_view(&base, 1));
        let mut text = base.clone();
        let mut state = EditorState::default();
        let mut live: std::collections::HashMap<(String, u32), u64> =
            std::collections::HashMap::new();
        let mut max_live = 0usize;
        let mut band_lines = 1usize;
        for frame in 0..200u64 {
            let line = 50 + (frame as usize % 100);
            let marker = format!("line {line:03}");
            let at = text.find(&marker).expect("marker line") + marker.len();
            let replacement = if frame % 2 == 0 { 'X' } else { 'x' };
            text.replace_range(at..at + 1, &replacement.to_string());
            let view = EditorView {
                revision: 2 + frame,
                text: flavite_buffer::chunks::DocumentText::from(text.as_str()),
                ..base_view.clone()
            };
            state.reconcile(&view, &HashMap::new());
            let shaped = state.sync_shaped(
                &DocumentText::from(text.as_str()),
                view.revision,
                viewport,
                width,
            );
            band_lines = shaped.line_starts.len();
            let paint_offset = shaped.paint_offset(viewport);
            let spans = band_line_spans(shaped.buffer, band_lines, line_height);
            let painted = painted_line_indices(&spans, paint_offset, paint_height);
            assert!(
                painted.len() <= band_lines,
                "painted per frame ≤ band lines"
            );
            live.clear();
            for index in painted {
                let key = (
                    band_line_content(shaped.visible, shaped.line_starts, index),
                    spans[index].1.to_bits(),
                );
                live.insert(key, frame);
            }
            max_live = max_live.max(live.len());
        }
        assert!(
            max_live <= 2 * band_lines + 8,
            "paragraph-key live set {max_live} exceeds 2×band+8"
        );
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
        let text = view.text.clone();
        state
            .selections
            .replace_with(&text, &[(text.len(), text.len())], 0);
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
        let text = view.text.clone();
        state
            .selections
            .replace_with(&text, &[(text.len(), text.len())], 0);
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
        let spans = crate::links::ts_structure_spans(text).0;
        let view = EditorView {
            buffer_id: 88,
            revision: 15,
            text: text.into(),
            path: std::path::PathBuf::from("source.md"),
            state: flavite_buffer::state::BufferState::Clean,
            last_edit: None,
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
        let byte = hit_test_visible(text, 0, 1, 45.0, 4.0, 200.0);
        assert!(text.is_char_boundary(byte));
    }

    #[test]
    fn cosmic_text_layout_contains_the_editor_text() {
        let text = "Heading\n\nbody\n\n- item\n- final visual editor input";
        let shaped = shape_text(text, 400.0, TypeMetrics::default());
        let runs = shaped.layout_runs().collect::<Vec<_>>();
        assert!(!runs.is_empty());
        assert!(runs.iter().any(|run| run.text == "Heading"));
        let starts = shape_line_starts(text);
        let (_, y) = caret_position(&runs, text, &starts, text.len(), 0.0);
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
            primary: 0,
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

    #[test]
    fn s1_observability_emits_render_spans_for_scan_and_shaping() {
        // RFC-0008 S1-T1 (render half): the widget emits `render.scan` on
        // revision reconcile and `render.shaping` on shaping. `render.paint`
        // emits on the real render path (driven by the S3 headless-renderer
        // double); the other FR-10 phases are asserted by the flavite-ui
        // keystroke-spans integration test.
        struct Recording(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);
        impl tracing::subscriber::Subscriber for Recording {
            fn register_callsite(
                &self,
                _metadata: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::always()
            }
            fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                self.0
                    .lock()
                    .expect("recorder lock")
                    .push(attrs.metadata().name());
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {
            }
            fn event(&self, _event: &tracing::Event<'_>) {}
            fn enter(&self, _span: &tracing::span::Id) {}
            fn exit(&self, _span: &tracing::span::Id) {}
            fn clone_span(&self, _span: &tracing::span::Id) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
        }

        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Recording(recorded.clone()), || {
            let mut state = EditorState::default();
            let view = EditorView::from(&test_view("one\ntwo\nthree\n", 77));
            state.reconcile(&view, &HashMap::new());
            let text = view.text.materialize();
            let shaped = state.sync_shaped(
                &DocumentText::from(text.as_ref()),
                view.revision,
                Viewport::default(),
                300.0,
            );
            let _ = shaped.visible; // the shaped view stays alive for the span
        });
        let names = recorded.lock().expect("recorder lock").clone();
        assert!(
            names.contains(&"render.scan"),
            "missing render.scan in {names:?}"
        );
        assert!(
            names.contains(&"render.shaping"),
            "missing render.shaping in {names:?}"
        );
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
    fn viewport_slice_maps_offsets_and_the_band_covers_the_viewport() {
        let source = "zero\none\ntwo\nthree\nfour";
        let viewport = Viewport {
            first_line: 2,
            visible_lines: 2,
            ..Viewport::default()
        };
        let (visible, source_start) = slice_lines(source, 2, 2);
        assert_eq!(visible, "two\nthree\n");
        assert_eq!(source_start, "zero\none\n".len());
        assert_eq!(
            hit_test_visible(source, 2, 2, 0.0, 1.0, 300.0),
            source_start
        );
        // The shaped band spans one viewport of margin around the viewport.
        let mut state = EditorState::default();
        let shaped = state.sync_shaped(&DocumentText::from(source), 1, viewport, 300.0);
        assert_eq!(shaped.band_start, 0);
        assert_eq!(shaped.visible, source);
        // A tall document's band is viewport-bounded on both sides.
        let tall = "line\n".repeat(200);
        let deep = Viewport {
            first_line: 100,
            visible_lines: 2,
            ..Viewport::default()
        };
        let band = state.sync_shaped(&DocumentText::from(tall.as_str()), 1, deep, 300.0);
        assert_eq!(band.band_start, 98);
        assert_eq!(band.source_start, 98 * "line\n".len());
        // Each band line is one row here, so the viewport start sits two
        // band rows down.
        assert_eq!(
            band.paint_offset(deep),
            2.0 * TypeMetrics::default().line_height
        );
        // Moving one line keeps the covering band; the paint offset then
        // skips exactly the one band row above the viewport.
        let high = Viewport {
            first_line: 99,
            ..deep
        };
        let band = state.sync_shaped(&DocumentText::from(tall.as_str()), 1, high, 300.0);
        assert_eq!(band.band_start, 98);
        assert_eq!(band.paint_offset(high), TypeMetrics::default().line_height);
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
            last_edit: None,
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
        let doc = DocumentText::from(text);
        state.ensure_caret_visible(&doc);
        assert_eq!(state.viewport.first_line, 2);

        state.selections.replace_with(text, &[(0, 0)], 0);
        state.ensure_caret_visible(&doc);
        assert_eq!(state.viewport.first_line, 0);
    }

    /// S8-T4 (structure paths at the widget layer): the engine path's spans
    /// equal the bridge path's spans equal the pulldown path's spans on
    /// representative docs — the AC-5 parity through the exact dispatch
    /// functions the widget uses, independent of the env flag (the engine
    /// is enabled/disabled by env; both branches are tested directly).
    #[test]
    fn structure_spans_engine_and_bridge_paths_match_pulldown() {
        let docs = [
            "# H [[link]] #tag ^block\n\nBody with ![[embed.png]] and `[[code]]`.\n",
            "- item [[a]]\n- [x] task #t\n\n| [[c]] | d |\n|---|---|\n| e | [[f]] |\n",
            "> quote [[q]]\n\n```\n[[fenced]]\n```\n",
        ];
        for doc in docs {
            // The bridge path (the shipped default routing).
            let bridge = bridge_structure_spans(doc);
            assert!(!bridge.0.is_empty(), "bridge must yield links: {doc}");
            let parsed = flavite_syntax::pulldown_glue::parse_normalized(doc).unwrap();
            let pulldown = crate::links::spans_from_pulldown(parsed);
            assert_eq!(bridge, pulldown, "bridge must equal pulldown: {doc}");

            // The engine path (flag-enabled routing, first sync is full).
            let lines = shape_line_starts(doc);
            let mut engine = flavite_syntax::StructureEngine::new();
            let engine_spans = engine_structure_spans(doc, None, &lines, &mut engine);
            assert_eq!(
                engine_spans, pulldown,
                "engine spans must equal pulldown: {doc}"
            );
        }
    }

    /// S7-T4 (plain mode): ≥ 64 MiB documents yield zero spans even when the
    /// text is full of links; the same content below the threshold yields
    /// spans, proving the gate — not the parser — is what emptied the result.
    /// The gate is flag-independent and precedes either structure source.
    #[test]
    fn structure_spans_plain_mode_gates_huge_documents() {
        let mut huge = String::with_capacity(HUGE_TEXT_THRESHOLD + 1024);
        while huge.len() < HUGE_TEXT_THRESHOLD {
            huge.push_str("[[link]] #tag ^block\n");
        }
        assert!(huge.len() >= HUGE_TEXT_THRESHOLD);

        let spans = structure_spans(&huge);
        assert!(
            spans.0.is_empty() && spans.1.is_empty(),
            "plain mode must yield zero spans"
        );

        // Same content below the threshold does produce spans — the gate is
        // length, not content. Uses the bridge path directly so the proof
        // does not depend on the engine flag.
        let small = &huge[..HUGE_TEXT_THRESHOLD - 1_000_000];
        let spans = bridge_structure_spans(small);
        assert!(!spans.0.is_empty() && !spans.1.is_empty());
    }

    /// S4-T1 (NFR-5): the widget performs zero full-document line-window
    /// queries on the typing path. The line-index machinery is gone
    /// (RFC-0011 S4): no full-document line-window query and no full-text
    /// newline scan exist in production code, and a 200-keystroke session
    /// keeps the only line vector band-local. The structural guards fail if
    /// a reintroduced full-document scan appears.
    #[test]
    fn s4_t1_typing_session_never_queries_a_full_document_line_window() {
        let source = include_str!("widget.rs");
        // Build the needles without embedding the literal (the test source
        // lives in the file it inspects).
        let line_window = format!("line{}starts_in(", "_");
        let index_scan = format!("match{}indices", "_");
        assert!(
            !source.contains(&line_window),
            "widget.rs must not query line windows (full-document line index removed)"
        );
        assert!(
            !source.contains(&index_scan),
            "widget.rs must not run newline-index scans"
        );

        // The 200-keystroke session: revision change → reconcile → shaping →
        // paint, exactly the widget's frame order. Band identity stays
        // viewport-bounded (the band vector is the only line vector).
        let base = (0..200u32)
            .map(|index| format!("line {index:03} alpha beta gamma delta\n"))
            .collect::<String>();
        let width = 800.0_f32;
        let line_height = TypeMetrics::default().line_height;
        let viewport = Viewport {
            first_line: 50,
            visible_lines: 15,
            ..Viewport::default()
        };
        let paint_height = 15.0 * line_height;
        let mut text = base.clone();
        let mut state = EditorState::default();
        for frame in 0..200u64 {
            let marker = format!("line {:03}", 50 + (frame as usize % 100));
            let at = text.find(&marker).expect("marker line") + marker.len();
            text.replace_range(at..at + 1, if frame % 2 == 0 { "X" } else { "x" });
            let view = EditorView {
                revision: 2 + frame,
                text: DocumentText::from(text.as_str()),
                ..long_view(0)
            };
            state.reconcile(&view, &HashMap::new());
            let painted = state.painted_line_count_for_perf(
                &view.text,
                view.revision,
                viewport,
                width,
                paint_height,
            );
            assert!(painted > 0, "paint paints lines");
            let shaped = state.sync_shaped(&view.text, view.revision, viewport, width);
            // The band stays viewport-bounded (15 visible lines + margins),
            // never document-sized.
            assert!(
                shaped.line_starts.len() <= 60,
                "band line vector must stay bounded, got {}",
                shaped.line_starts.len()
            );
        }
        assert!(state.revision.is_some());
    }

    /// S4-T2 (A8): the two-finger decoration walk decides exactly what the
    /// reference per-glyph `.find`/`.any` scan decides, on randomized
    /// non-overlapping span sets and glyph windows.
    #[test]
    fn s4_t2_two_finger_decoration_walk_matches_the_reference_scan() {
        fn reference(
            links: &[LinkSpan],
            decos: &[flavite_syntax::ScannedSpan],
            glyphs: &[(usize, usize)],
        ) -> Vec<(Option<LinkResolution>, bool)> {
            glyphs
                .iter()
                .map(|&(gs, ge)| {
                    let link = links
                        .iter()
                        .find(|span| span.span.start < ge && span.span.end > gs)
                        .map(|span| span.resolution);
                    let deco = decos
                        .iter()
                        .any(|span| span.span.start.0 < ge && span.span.end.0 > gs);
                    (link, deco)
                })
                .collect()
        }

        fn link_span(start: usize, end: usize, resolution: LinkResolution) -> LinkSpan {
            LinkSpan {
                span: start..end,
                payload: flavite_syntax::scanner::LinkPayload {
                    target_raw: "t".into(),
                    alias: None,
                    anchor: None,
                    by_id: false,
                },
                embed: false,
                resolution,
            }
        }

        fn deco_span(start: usize, end: usize) -> flavite_syntax::ScannedSpan {
            flavite_syntax::ScannedSpan {
                span: flavite_syntax::text::Span::new(
                    flavite_syntax::TextByte(start),
                    flavite_syntax::TextByte(end),
                ),
                kind: flavite_syntax::scanner::SpanKind::Tag,
                payload: None,
            }
        }

        // Deterministic corpus: overlapping-free, source-ordered spans with
        // gaps, plus a glyph window that starts inside a span and ends
        // inside another.
        let links = [
            link_span(2, 6, LinkResolution::Resolved),
            link_span(10, 14, LinkResolution::MultipleCandidates),
            link_span(20, 30, LinkResolution::Unresolved),
        ];
        let decos = [
            deco_span(0, 3),
            deco_span(8, 9),
            deco_span(25, 27),
            deco_span(40, 45),
        ];
        let glyphs: Vec<(usize, usize)> = (0..50).map(|i| (i, i + 1)).collect();
        assert_eq!(
            per_glyph_decorations(&links, &decos, glyphs.iter().copied()),
            reference(&links, &decos, &glyphs)
        );

        proptest::proptest!(|(spans in prop::collection::vec(
            (0usize..400, 1usize..12, 0usize..3),
            0..12,
        ))| {
            // Sort and de-overlap to build a valid span set.
            let mut sorted = spans;
            sorted.sort_unstable_by_key(|&(s, _, _)| s);
            let mut links = Vec::new();
            let mut decos = Vec::new();
            let mut cursor = 0usize;
            for (start, len, kind) in sorted {
                let start = start.max(cursor);
                let end = start + len;
                if kind == 0 {
                    links.push(link_span(start, end, LinkResolution::Unresolved));
                } else {
                    decos.push(deco_span(start, end));
                }
                cursor = end;
            }
            let glyphs: Vec<(usize, usize)> = (0..420).step_by(3).map(|i| (i, i + 2)).collect();
            prop_assert_eq!(
                per_glyph_decorations(&links, &decos, glyphs.iter().copied()),
                reference(&links, &decos, &glyphs)
            );
        });
    }

    /// S4-T3 (AC-6): plain-mode (≥64 MiB) typing does zero structure work
    /// and zero full-document materialization; the tree serves band slicing.
    #[test]
    fn s4_t3_plain_mode_huge_document_zero_structure_zero_scans() {
        let mut huge = String::with_capacity(HUGE_TEXT_THRESHOLD + 4096);
        while huge.len() < HUGE_TEXT_THRESHOLD {
            huge.push_str("[[link]] #tag ^block\n");
        }
        assert!(huge.len() >= HUGE_TEXT_THRESHOLD);

        let viewport = Viewport {
            first_line: 0,
            visible_lines: 3,
            ..Viewport::default()
        };
        let mut state = EditorState::default();
        let mut view = EditorView {
            buffer_id: 1,
            revision: 1,
            text: DocumentText::from(huge.as_str()),
            path: std::path::PathBuf::from("huge.md"),
            state: BufferState::Clean,
            last_edit: None,
        };
        state.reconcile(&view, &HashMap::new());
        // Plain mode: zero structure spans.
        assert!(state.link_spans.is_empty() && state.decorations.is_empty());
        // Zero materialization: the per-revision text cache is untouched.
        assert!(
            state.materialized_text.borrow().is_none(),
            "plain-mode typing must not materialize the document"
        );

        // Band slicing is tree-backed and matches the source slice.
        let shaped = state.sync_shaped(&view.text, view.revision, viewport, 800.0);
        assert_eq!(shaped.band_start, 0);
        assert_eq!(
            shaped.visible,
            &huge[..shaped.visible.len()],
            "band text equals the source slice"
        );

        // Typing/undo stay functional: a few keystrokes reconcile new
        // revisions with zero structure work and zero materialization.
        let mut typed = huge.clone();
        for step in 0..10u64 {
            typed.insert(0, 'x');
            view.revision += 1;
            view.text = DocumentText::from(typed.as_str());
            state.reconcile(&view, &HashMap::new());
            assert!(state.link_spans.is_empty());
            assert!(state.decorations.is_empty());
            assert!(
                state.materialized_text.borrow().is_none(),
                "keystroke {step} must not materialize"
            );
            let _ = state.sync_shaped(&view.text, view.revision, viewport, 800.0);
        }
    }
}
