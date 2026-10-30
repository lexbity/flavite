//! Custom iced editor widget over a revisioned, immutable API view.

use crate::keymap::KeyAction;
use crate::links::{
    LinkResolution, LinkSpan, NoteCandidate, match_candidates, resolve_link, scan_decorations,
    scan_links,
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
    candidates: Vec<NoteCandidate>,
    link_resolutions: HashMap<String, LinkResolution>,
    stable_links: bool,
    on_action: Box<dyn Fn(EditorAction) -> Message + 'a>,
    on_interaction: Option<Box<dyn Fn(EditorInteractionSnapshot) -> Message + 'a>>,
    width: Length,
    height: Length,
    _message: PhantomData<Message>,
}

impl<'a, Message> EditorWidget<'a, Message> {
    /// Create an editor from the API's immutable revisioned view.
    pub fn new(view: &BufferView, on_action: impl Fn(EditorAction) -> Message + 'a) -> Self {
        Self {
            view: EditorView::from(view),
            candidates: Vec::new(),
            link_resolutions: HashMap::new(),
            stable_links: false,
            on_action: Box::new(on_action),
            on_interaction: None,
            width: Length::Fill,
            height: Length::Fill,
            _message: PhantomData,
        }
    }

    /// Supply an indexed title/alias snapshot for in-memory autocomplete.
    #[must_use]
    pub fn with_candidates(mut self, candidates: Vec<NoteCandidate>) -> Self {
        self.candidates = candidates;
        self
    }

    /// Supply resolver verdicts keyed by the folded raw link target. Verdicts
    /// come from the index snapshot API; the editor does not rank candidates.
    #[must_use]
    pub fn with_link_resolutions(mut self, resolutions: HashMap<String, LinkResolution>) -> Self {
        self.link_resolutions = resolutions
            .into_iter()
            .map(|(target, verdict)| (flavite_syntax::text::fold_key(&target), verdict))
            .collect();
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
                        let inserted = format_link(candidate, self.stable_links);
                        self.emit(
                            shell,
                            EditorAction::insert_link(
                                &self.view,
                                start,
                                state.caret,
                                inserted.clone(),
                            ),
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
    buffer_id: Option<u64>,
    revision: Option<u64>,
    link_spans: Vec<LinkSpan>,
    decorations: Vec<flavite_syntax::ScannedSpan>,
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
}

/// Host-visible per-view interaction state needed by accessibility and
/// composition-aware workspace commands. The text itself remains owned by the
/// revisioned buffer view and is not copied into widget state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorInteractionSnapshot {
    pub buffer_id: u64,
    pub revision: u64,
    pub caret: usize,
    pub selection: (usize, usize),
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
                buffer_id,
                revision,
                caret: state.caret,
                selection: state.selection(),
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
            self.result = state.interaction_snapshot();
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
            self.result = state.interaction_snapshot();
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
    pub fn is_composing(&self) -> bool {
        self.composition.is_some()
    }

    fn interaction_snapshot(&self) -> Option<EditorInteractionSnapshot> {
        Some(EditorInteractionSnapshot {
            buffer_id: self.buffer_id?,
            revision: self.revision?,
            caret: self.caret,
            selection: self.selection(),
            focused: self.focused,
            composing: self.is_composing(),
        })
    }

    fn input_method_event(
        &mut self,
        view: &EditorView,
        event: &input_method::Event,
    ) -> Option<EditorAction> {
        match event {
            input_method::Event::Opened => {
                self.ime_active = true;
                let (start, end) = self.selection();
                self.composition = Some(Composition {
                    start,
                    end,
                    preedit: String::new(),
                    selection: None,
                });
                None
            }
            input_method::Event::Preedit(preedit, selection) => {
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

    fn reconcile(&mut self, view: &EditorView, link_resolutions: &HashMap<String, LinkResolution>) {
        if self
            .buffer_id
            .is_some_and(|buffer_id| buffer_id != view.buffer_id)
        {
            *self = Self::default();
        }
        self.buffer_id = Some(view.buffer_id);
        if self.revision != Some(view.revision) {
            self.link_spans = scan_links(&view.text);
            self.decorations = scan_decorations(&view.text);
            self.revision = Some(view.revision);
        }
        for span in &mut self.link_spans {
            let key = flavite_syntax::text::fold_key(&span.payload.target_raw);
            span.resolution = resolve_link(span, link_resolutions.get(&key).copied());
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
        // Use the semantic scanner for link decorations. No markdown parser or
        // second link grammar is embedded in the widget.
        for run in &runs {
            let line_start = source_start + line_start_byte(&visible_text, run.line_i);
            for glyph in run.glyphs {
                let glyph_start = line_start + glyph.start;
                let glyph_end = line_start + glyph.end;
                let link_resolution = state
                    .link_spans
                    .iter()
                    .find(|span| span.span.start < glyph_end && span.span.end > glyph_start)
                    .map(|span| span.resolution);
                let other_decoration = state.decorations.iter().any(|span| {
                    matches!(span.kind, SpanKind::Tag | SpanKind::BlockId)
                        && span.span.start.0 < glyph_end
                        && span.span.end.0 > glyph_start
                });
                if link_resolution.is_some() || other_decoration {
                    let color = match link_resolution {
                        Some(LinkResolution::Unresolved) => Color::from_rgb(0.95, 0.48, 0.43),
                        Some(LinkResolution::MultipleCandidates) => {
                            Color::from_rgb(0.96, 0.73, 0.32)
                        }
                        _ => Color::from_rgb(0.35, 0.67, 0.98),
                    };
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
        if let Some(composition) = &state.composition
            && !composition.preedit.is_empty()
            && composition.start >= source_start
            && composition.start <= source_start + visible_text.len()
        {
            let local_start = composition.start - source_start;
            let (x, y) = caret_position(&runs, &visible_text, local_start);
            let preedit_layout =
                shape_text(&composition.preedit, (bounds.width - PAD_X * 2.0).max(1.0));
            let preedit_width = preedit_layout
                .layout_runs()
                .map(|run| run.glyphs.iter().map(|glyph| glyph.w).sum::<f32>())
                .fold(0.0_f32, f32::max);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(
                            bounds.x + PAD_X + x,
                            bounds.y + PAD_Y + y + LINE_HEIGHT - 2.0,
                        ),
                        Size::new(preedit_width.max(1.0), 1.5),
                    ),
                    border: Border::default(),
                    ..renderer::Quad::default()
                },
                Background::Color(Color::from_rgb(0.98, 0.78, 0.35)),
            );
            renderer.fill_text(
                text::Text {
                    content: composition.preedit.clone(),
                    bounds: Size::new((bounds.width - PAD_X * 2.0).max(1.0), LINE_HEIGHT),
                    size: iced::Pixels(FONT_SIZE),
                    line_height: text::LineHeight::Absolute(iced::Pixels(LINE_HEIGHT)),
                    font: iced::Font::MONOSPACE,
                    align_x: text::Alignment::Left,
                    align_y: iced::alignment::Vertical::Top,
                    shaping: text::Shaping::Advanced,
                    wrapping: text::Wrapping::None,
                },
                Point::new(bounds.x + PAD_X + x, bounds.y + PAD_Y + y),
                Color::from_rgb(1.0, 0.86, 0.55),
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
                                * LINE_HEIGHT,
                    ),
                    Size::new(320.0, row_count as f32 * 23.0 + 8.0),
                );
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: popup,
                        border: Border::default(),
                        ..renderer::Quad::default()
                    },
                    Background::Color(Color::from_rgb(0.15, 0.17, 0.20)),
                );
                for (index, candidate) in matches.iter().enumerate() {
                    let color = if index == state.autocomplete_selected {
                        Color::from_rgb(0.55, 0.78, 1.0)
                    } else {
                        Color::from_rgb(0.82, 0.84, 0.87)
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
                        Color::from_rgb(0.55, 0.78, 1.0)
                    } else {
                        Color::from_rgb(0.82, 0.84, 0.87)
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
            .reconcile(&self.view, &self.link_resolutions);
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
                    let byte = hit_test_visible(
                        &self.view.text,
                        state.viewport,
                        position.x - PAD_X,
                        position.y - PAD_Y,
                        (layout.bounds().width - PAD_X * 2.0).max(1.0),
                    );
                    if (state.modifiers.command() || state.modifiers.control())
                        && let Some(action) = link_action_at(&self.view, &state.link_spans, byte)
                    {
                        self.emit(shell, action);
                        shell.capture_event();
                        return;
                    }
                    state.focused = true;
                    state.ime_active = true;
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
                    state.ime_active = false;
                    state.selecting = false;
                    state.composition = None;
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
            }) if state.focused
                && state.composition.is_none()
                && self.execute_key(state, key, *modifiers, text.as_deref(), clipboard, shell) =>
            {
                shell.capture_event();
            }
            _ => {}
        }
        if state.focused && state.ime_active {
            let (visible, source_start) = visible_text(&self.view.text, state.viewport);
            let layout_text = shape_text(&visible, (layout.bounds().width - PAD_X * 2.0).max(1.0));
            let runs = layout_text.layout_runs().collect::<Vec<_>>();
            let caret = state.caret.saturating_sub(source_start).min(visible.len());
            let (x, y) = caret_position(&runs, &visible, caret);
            shell.request_input_method(&InputMethod::Enabled {
                cursor: Rectangle::new(
                    Point::new(layout.bounds().x + PAD_X + x, layout.bounds().y + PAD_Y + y),
                    Size::new(1.0, LINE_HEIGHT),
                ),
                purpose: input_method::Purpose::Normal,
                // The widget paints preedit inline itself. Sending a runtime
                // preedit as well would draw a second over-the-spot copy.
                preedit: None::<input_method::Preedit<&str>>,
            });
        } else {
            shell.request_input_method::<&str>(&InputMethod::Disabled);
        }
        if let Some(on_interaction) = &self.on_interaction
            && let Some(snapshot) = state.interaction_snapshot()
        {
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
            mouse::Interaction::Text
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

fn format_link(candidate: &NoteCandidate, stable_links: bool) -> String {
    if stable_links && let Some(id) = &candidate.stable_id {
        return format!("[[id:{id}|{}]]", candidate.title);
    }
    if candidate.target == candidate.title {
        format!("[[{}]]", candidate.target)
    } else {
        format!("[[{}|{}]]", candidate.target, candidate.title)
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

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_syntax::text::{Span, TextByte};
    use iced::advanced::widget::Operation as _;

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
        assert_eq!(action.expected_revision, view.revision);
        assert_eq!(action.edits.len(), 1);
        assert_eq!(action.edits[0].range, Span::new(TextByte(2), TextByte(2)));
        assert_eq!(action.edits[0].replacement, "日本語");
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
        assert_eq!(committed.edits.len(), 1);
        assert_eq!(
            committed.edits[0].range,
            Span::new(TextByte(3), TextByte(3))
        );
        assert_eq!(committed.edits[0].replacement, "仮名");
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
        let widget = EditorWidget::new(&view, |action| action).with_candidates(vec![
            NoteCandidate::new("notes/inbox", "Inbox"),
            NoteCandidate::new("Ideas", "Ideas"),
        ]);
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
        assert_eq!(messages[0].expected_revision, 11);
        assert_eq!(messages[0].command_id, "link.insert");
        assert_eq!(
            messages[0].edits[0].range,
            Span::new(TextByte(0), TextByte(4))
        );
        assert_eq!(messages[0].edits[0].replacement, "[[notes/inbox|Inbox]]");
        let invocation = messages[0]
            .clone()
            .into_invocation(88)
            .expect("registry invocation");
        assert_eq!(invocation.command_id, "link.insert");
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
            .with_candidates(vec![NoteCandidate::new("Ideas", "Ideas")]);
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
        assert_eq!(messages[0].command_id, "note.open");
        let navigation = messages[0].navigation.as_ref().expect("create navigation");
        assert_eq!(navigation.target, "New note");
        assert!(navigation.create_if_unresolved);
        assert_eq!(navigation.span, Span::new(TextByte(0), TextByte(5)));
        assert!(state.autocomplete_start.is_none());
    }

    #[test]
    fn link_insert_supports_readable_aliases_and_opt_in_stable_form() {
        let mut candidate = NoteCandidate::new("notes/concepts", "Concepts");
        candidate.stable_id = Some("01J9ZK".into());
        assert_eq!(
            format_link(&candidate, false),
            "[[notes/concepts|Concepts]]"
        );
        assert_eq!(format_link(&candidate, true), "[[id:01J9ZK|Concepts]]");
    }

    #[test]
    fn link_activation_emits_target_syntax_and_never_opens_embeds_as_notes() {
        let text = "[[Ideas#Plan|Ideas]] ![[diagram.png]]";
        let spans = scan_links(text);
        let view = EditorView {
            buffer_id: 88,
            revision: 15,
            text: text.into(),
            path: std::path::PathBuf::from("source.md"),
            state: "Clean".into(),
        };
        let action = link_action_at(&view, &spans, 4).expect("link activation");
        assert_eq!(action.buffer_id, 88);
        assert_eq!(action.expected_revision, 15);
        assert!(action.edits.is_empty());
        let navigation = action.navigation.expect("navigation action");
        assert_eq!(navigation.target, "Ideas");
        assert_eq!(navigation.alias.as_deref(), Some("Ideas"));
        assert_eq!(navigation.anchor.as_deref(), Some("Plan"));
        assert!(navigation.create_if_unresolved);
        assert!(link_action_at(&view, &spans, text.find("diagram").unwrap()).is_none());
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
