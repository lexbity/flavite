//! Context Mode's statechart (FEUX-0002 E-2/E-3, §5.4): mode, menus,
//! selection cycling, and session messages for one editor view.
//!
//! Every state mutation goes through this module — the host never writes
//! these fields directly. The host classifies admission against live app
//! state, forwards [`Event`]s to [`reduce`], and interprets the returned
//! [`Effect`]s plus the state accessors. Exit interleaves host-owned effects
//! (sealing, find/palette teardown, generation bump), so the pure reset is
//! [`ContextState::exit`] rather than an event; every other transition of
//! FEUX-0002 §5.4 is expressed here.
//!
//! Message strings are behavior-locked (X02 moved them verbatim; the
//! delivered suites assert them).

use crate::vocabulary::{ContextCommand, ContextMenu, ContextMode};
use std::collections::VecDeque;

/// Pending command awaiting argument capture (FEUX-0004 E-6).
/// The generalization of `AwaitChar` to support per-command argument specs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCommand {
    /// `FindChar` with direction (forward/backward).
    FindChar { forward: bool },
    /// `SurroundAdd`: awaiting a single character to wrap with.
    SurroundAdd,
    /// `SurroundChange`: awaiting first character of the pair.
    SurroundChange,
    /// `SurroundDelete`: awaiting the character of the pair to delete.
    SurroundDelete,
}

impl PendingCommand {
    /// Returns the number of arguments this command expects.
    #[must_use]
    pub const fn arg_count(&self) -> usize {
        match self {
            PendingCommand::FindChar { .. }
            | PendingCommand::SurroundAdd
            | PendingCommand::SurroundDelete => 1,
            PendingCommand::SurroundChange => 2,
        }
    }

    /// Returns the message to display for the given index.
    #[must_use]
    pub fn message_for_index(&self, index: usize) -> &'static str {
        if matches!(self, PendingCommand::SurroundChange) && index == 1 {
            AWAIT_SECOND_CHAR_HINT
        } else {
            AWAIT_CHAR_HINT
        }
    }
}

/// Message constants for argument capture (FEUX-0004 E-6).
pub const AWAIT_CHAR_HINT: &str = "Waiting for character…";
pub const AWAIT_SECOND_CHAR_HINT: &str = "Waiting for second character…";

/// Count cap (FEUX-0004 E-4).
pub const COUNT_CAP: u32 = 10_000;

/// Message constants for count state (FEUX-0004 E-4).
pub const COUNT_CAP_REFUSAL: &str = "Count is capped at 10000";

/// Burst-hint thresholds (FEUX-0002 E-15, X07): the hint offers Exit
/// Context Mode and Open history after ≥5 accelerator activations within
/// 1.5 s of which ≥4 are mutating or typing-entry, at most once per 30 s
/// per session. These are constants here; the only settings knob disables
/// the hint locally (the host passes `burst_enabled` into [`reduce`]).
pub const BURST_WINDOW_MS: u64 = 1500;
pub const BURST_MIN_ACTIVATIONS: usize = 5;
pub const BURST_MIN_MUTATING: usize = 4;
pub const BURST_SUPPRESS_MS: u64 = 30_000;

/// One accelerator activation inside the sliding burst window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BurstActivation {
    at_ms: u64,
    /// Whether the activation mutates text or enters typing (the vocabulary
    /// classification, E-15).
    mutating: bool,
}

/// Invocation record for the last executed command (FEUX-0004 E-5).
/// Records the command id and its arguments for replay via `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationRecord {
    /// The canonical command id (must be &'static for replay).
    pub command_id: &'static str,
    /// The arguments passed to the command.
    /// For typing-entry commands, this contains the `typed_text`.
    /// For surround commands, this contains the pair characters.
    /// For align, this contains the delimiter.
    pub args: std::collections::BTreeMap<String, String>,
}

/// The pure session state of one editor's Context Mode. Never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextState {
    mode: ContextMode,
    menu: Option<ContextMenu>,
    selected: usize,
    /// The pending argument capture; `None` outside argument collection.
    /// A sub-state of Command, not a mode of its own (E-8/E-6).
    /// Replaces the former `await_char: Option<AwaitChar>` (FEUX-0004 X04, E-6).
    await_args: Option<AwaitArgs>,
    message: String,
    attention: bool,
    /// The sliding activation window driving the burst hint (X07, E-15).
    /// Not a mode; it records admitted activations only.
    burst_window: VecDeque<BurstActivation>,
    /// The last time a burst hint was emitted, in the same clock as
    /// `now_ms` (suppression window).
    last_burst_ms: u64,
    /// The last recorded invocation for `.` repeat (FEUX-0004 E-5).
    /// Lives and dies with the CM session; never persisted.
    pub repeat_last: Option<InvocationRecord>,
    /// Pending numeric prefix count (FEUX-0004 E-4).
    /// Grammar-owned digits accumulate here before being consumed by an acceptor command.
    /// Cap: 10,000; leading 0 is not a count (dispatches as unbound).
    pub count: Option<u32>,
}

/// What [`ContextState::escape`] resolved to (FEUX-0002 §5.4 rows 9-10 and
/// the delivered menu pop: a menu always pops before typing leaves).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeOutcome {
    /// An open menu closed; the mode is unchanged.
    PoppedMenu,
    /// Typing ended; the mode is Command.
    LeftTyping,
    /// Command Escape with more than one member: the host keeps the primary
    /// (row 10's multi-selection recovery); the mode is unchanged.
    KeptPrimary,
    /// No menu and not typing: the mode is unchanged.
    StayedCommand,
}

/// The pending argument capture state (FEUX-0004 E-6).
/// Generalizes the former `AwaitChar` to support per-command argument collection.
/// The command specifies how many arguments it needs and what they mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitArgs {
    /// The command awaiting arguments.
    pub command: PendingCommand,
    /// The current argument index being collected (0-based).
    pub index: usize,
    /// The characters collected so far.
    pub collected_chars: Vec<char>,
    /// The pending count at the time of entry (FEUX-0004 X06, E-4).
    /// Retained through argument capture for count acceptors like `find_char`.
    pub count: Option<u32>,
}

impl AwaitArgs {
    /// Create a new `AwaitArgs` for a find-char command.
    #[must_use]
    pub fn find_char(forward: bool) -> Self {
        Self {
            command: PendingCommand::FindChar { forward },
            index: 0,
            collected_chars: Vec::new(),
            count: None,
        }
    }

    /// Create a new `AwaitArgs` for surround add.
    #[must_use]
    pub fn surround_add() -> Self {
        Self {
            command: PendingCommand::SurroundAdd,
            index: 0,
            collected_chars: Vec::new(),
            count: None,
        }
    }

    /// Create a new `AwaitArgs` for surround change.
    #[must_use]
    pub fn surround_change() -> Self {
        Self {
            command: PendingCommand::SurroundChange,
            index: 0,
            collected_chars: Vec::new(),
            count: None,
        }
    }

    /// Create a new `AwaitArgs` for surround delete.
    #[must_use]
    pub fn surround_delete() -> Self {
        Self {
            command: PendingCommand::SurroundDelete,
            index: 0,
            collected_chars: Vec::new(),
            count: None,
        }
    }

    /// Get the message to display.
    #[must_use]
    pub fn message(&self) -> &'static str {
        self.command.message_for_index(self.index)
    }
}

/// Statechart events the host forwards after its admission classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The view activates into Command (header button admitted, right-click
    /// `EnterAt` admitted, or a Toggle enter).
    Activate,
    /// An admitted toolbar/menu/accelerator command starts executing.
    RunAdmitted(ContextCommand),
    /// An admitted typing-entry command begins its typing sub-state.
    EnterTyping,
    /// An admitted command begins argument capture (FEUX-0004 E-6).
    /// Replaces `EnterAwaitChar` — find-char now uses `EnterAwaitArgs(FindChar { forward })`.
    EnterAwaitArgs(AwaitArgs),
    /// Record an invocation for `.` repeat (FEUX-0004 E-5).
    /// The host sends this after a class-E command completes successfully.
    Record(InvocationRecord),
    /// Escape was admitted while no owned overlay consumed it. The host
    /// supplies the live selection cardinality: Command Escape with more
    /// than one member keeps the primary (§5.4 row 10).
    Escape { selections: usize },
    /// An admitted menu key opens a menu.
    OpenMenu(ContextMenu),
    /// The menu closes (focus returns to the editor).
    CloseMenu,
    /// Menu selection moves by one row (wrapping).
    MenuMove(bool),
    /// A pointer/motion boundary changed the selection outside the toolbar.
    ExternalSelectionChanged,
    /// The reconcile pass found the open menu's target stale.
    MenuTargetInvalid,
    /// The OS window lost input focus (typing seals, mode retained).
    Blur,
    /// A digit key was pressed in Command mode (FEUX-0004 E-4).
    /// The host pre-classifies digits before keymap lookup; this event carries
    /// the resolved digit value for the statechart to accumulate.
    CountDigit(u32),
    /// The count is consumed by a dispatch the statechart does not see
    /// (FEUX-0004 E-4: motions route through the native path; unbound and
    /// stale refusals consume too). Clearing is a side-effect-free event so
    /// the host can consume without fabricating a `RunAdmitted`.
    ClearCount,
}

/// Pure outcomes the host interprets against the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Close the open typing undo group (delivered `seal`).
    SealTypingGroup,
    /// Drop the toolbar hover preview.
    ClearPreview,
    /// Return keyboard focus to the source editor.
    FocusEditor,
    /// Refresh the editing-history chooser (selection moved or ran).
    InvalidateHistory,
    /// The burst threshold was freshly crossed: show the non-blocking hint
    /// (FEUX-0002 E-15, X07). Emitted at most once per 30 s; the host
    /// renders the banner with Exit Context Mode and Open history actions.
    ShowBurstHint,
    /// Command Escape with more than one member (§5.4 row 10): the host
    /// runs `editor.keep_primary` so the user can discard additional
    /// targets before ordinary typing.
    KeepPrimary,
}

impl ContextState {
    /// The delivered activation state: Command with the intro hint.
    #[must_use]
    pub fn activated() -> Self {
        Self {
            mode: ContextMode::Command,
            message: Self::INTRO.into(),
            ..Self::default()
        }
    }

    pub const INTRO: &str = "Select text, then act · Esc returns to commands";
    pub const TYPING_HINT: &str = "Typing · click or move to reposition · Esc returns to commands";
    pub const ESC_HINT: &str = "Commands · exit with ✕ or the toggle shortcut";
    pub const UNBOUND: &str = "No command on this key · choose a menu";
    pub const AWAIT_SECOND_CHAR_HINT: &str = "Waiting for second character…";
    pub const MENU_TARGET_STALE: &str = "Source changed; choose the command again";

    #[must_use]
    pub const fn mode(&self) -> ContextMode {
        self.mode
    }

    #[must_use]
    pub const fn menu(&self) -> Option<ContextMenu> {
        self.menu
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// The pending argument capture, if the statechart is in argument collection.
    /// Returns a clone of the [`AwaitArgs`] if present.
    #[must_use]
    pub fn await_args(&self) -> Option<AwaitArgs> {
        self.await_args.clone()
    }

    /// Enter argument capture (§5.4.1 row 5, FEUX-0004 E-6): the state row
    /// shows the capture hint for the first argument.
    pub fn begin_await_args(&mut self, args: AwaitArgs) {
        debug_assert!(
            self.mode.active(),
            "AwaitArgs entry requires an active mode"
        );
        let message = args.message().into();
        self.await_args = Some(args);
        self.message = message;
        self.attention = false;
    }

    /// Cancel the argument capture; returns whether one was pending.
    pub fn cancel_await_args(&mut self) -> bool {
        self.await_args.take().is_some()
    }

    /// Add a collected character to the argument capture.
    /// If this completes the collection, returns Some with the collected chars.
    /// Otherwise returns None and advances to the next argument.
    pub fn collect_char(&mut self, char: char) -> Option<Vec<char>> {
        let current = self.await_args.take()?;
        let mut collected_chars = current.collected_chars;
        collected_chars.push(char);

        if collected_chars.len() >= current.command.arg_count() {
            // All arguments collected
            self.await_args = None;
            Some(collected_chars)
        } else {
            // More arguments needed
            let new_args = AwaitArgs {
                index: current.index + 1,
                collected_chars,
                ..current
            };
            let message = new_args.message().into();
            self.await_args = Some(new_args);
            self.message = message;
            self.attention = false;
            None
        }
    }

    /// Get the collected characters without consuming.
    #[must_use]
    pub fn collected_chars(&self) -> Option<&[char]> {
        self.await_args
            .as_ref()
            .map(|args| args.collected_chars.as_slice())
    }

    /// The current message and whether it demands attention (an error
    /// notice) versus being plain guidance.
    #[must_use]
    pub fn message(&self) -> (&str, bool) {
        (&self.message, self.attention)
    }

    /// Whether the current message demands attention.
    #[must_use]
    pub const fn attention(&self) -> bool {
        self.attention
    }

    /// Drop the attention flag without touching the message (delivered
    /// host/input behavior when an acknowledged edit lands).
    pub fn clear_attention(&mut self) {
        self.attention = false;
    }

    pub fn set_message(&mut self, text: impl Into<String>, attention: bool) {
        self.message = text.into();
        self.attention = attention;
    }

    /// Get the last recorded invocation for `.` repeat (FEUX-0004 E-5).
    #[must_use]
    pub fn repeat_last(&self) -> Option<&InvocationRecord> {
        self.repeat_last.as_ref()
    }

    /// Get the current pending count (FEUX-0004 E-4).
    #[must_use]
    pub const fn count(&self) -> Option<u32> {
        self.count
    }

    /// Set the pending count (FEUX-0004 E-4).
    pub fn set_count(&mut self, count: Option<u32>) {
        self.count = count;
    }

    /// Clear the pending count (FEUX-0004 E-4).
    pub fn clear_count(&mut self) {
        self.count = None;
    }

    /// The command the open menu's selection points at.
    #[must_use]
    pub fn menu_command(&self) -> Option<ContextCommand> {
        self.menu
            .and_then(|menu| menu.commands().get(self.selected).copied())
    }

    /// Open a menu with its first row selected.
    pub fn open_menu(&mut self, menu: ContextMenu) {
        self.menu = Some(menu);
        self.selected = 0;
    }

    /// Close an open menu; returns whether one was open.
    pub fn close_menu(&mut self) -> bool {
        self.menu.take().is_some()
    }

    /// Cycle the menu selection; returns the new row index. Wraps at both
    /// ends (delivered `MenuMove`).
    pub fn menu_move(&mut self, forward: bool) -> usize {
        let Some(menu) = self.menu else {
            return self.selected;
        };
        let n = menu.commands().len();
        self.selected = if forward {
            (self.selected + 1) % n
        } else {
            (self.selected + n - 1) % n
        };
        self.selected
    }

    /// Escape pops exactly one level: a menu first, then typing, then argument
    /// capture, and at Command it stays — keeping the primary when the set has
    /// more than one member (§5.4 row 10: the multi-selection recovery). The host
    /// shows the exit hint in every Command outcome.
    pub fn escape(&mut self, selections: usize) -> EscapeOutcome {
        if self.menu.take().is_some() {
            EscapeOutcome::PoppedMenu
        } else if self.await_args.take().is_some() {
            EscapeOutcome::StayedCommand
        } else if self.mode == ContextMode::Typing {
            self.mode = ContextMode::Command;
            EscapeOutcome::LeftTyping
        } else if selections > 1 {
            EscapeOutcome::KeptPrimary
        } else {
            EscapeOutcome::StayedCommand
        }
    }

    /// An admitted command starts: menus close and the message names it.
    pub fn admit_run(&mut self, command: ContextCommand) {
        self.menu = None;
        self.selected = 0;
        self.await_args = None;
        self.message = command.title().into();
        self.attention = false;
    }

    /// An admitted typing-entry command switches to Typing with its hint.
    /// The host enters the mode first when it was off; this only runs while
    /// the mode is active.
    pub fn begin_typing(&mut self) {
        debug_assert!(self.mode.active(), "typing entries require an active mode");
        self.mode = ContextMode::Typing;
        self.message = Self::TYPING_HINT.into();
    }

    /// A pointer/motion boundary outside the toolbar closes an open menu
    /// (delivered `selection_changed`: the menu target itself is left for
    /// reconcile to retire).
    pub fn external_selection_changed(&mut self) {
        self.menu = None;
    }

    /// The reconcile pass found the menu's captured target stale.
    pub fn menu_target_invalid(&mut self) {
        self.menu = None;
        self.message = Self::MENU_TARGET_STALE.into();
    }

    /// The pure reset of a leaving session (mode off, no menu, no message).
    /// The host performs its own teardown around this.
    pub fn exit(&mut self) {
        *self = Self::default();
    }
}

/// The statechart entry point: apply one event, returning the effects the
/// host must execute in order. Events are pre-admitted by the host; `reduce`
/// never checks application state.
///
/// `now_ms` is the host's monotonic millisecond clock (injected so the pure
/// chart is fully deterministic under test) and `burst_enabled` is the
/// resolved `editor.burst_hint_enabled` setting (X07). Both only affect the
/// burst-hint window; they never change a transition.
#[must_use]
pub fn reduce(
    state: &mut ContextState,
    event: &Event,
    now_ms: u64,
    burst_enabled: bool,
) -> Vec<Effect> {
    match event {
        Event::Activate => {
            *state = ContextState::activated();
            Vec::new()
        }
        Event::RunAdmitted(command) => {
            // FEUX-0004 E-4: dispatch consumes/clears the count even when refused.
            state.clear_count();
            state.admit_run(*command);
            burst_effect(state, Some(*command), now_ms, burst_enabled)
        }
        Event::EnterTyping => {
            // FEUX-0004 E-4: EnterTyping clears the count.
            state.clear_count();
            state.begin_typing();
            // Every typing entry arrives through an admitted Run first (the
            // host contract), so the activation was already recorded there;
            // recording it again would double-count one physical action.
            Vec::new()
        }
        Event::EnterAwaitArgs(args) => {
            state.begin_await_args(args.clone());
            Vec::new()
        }
        Event::Record(record) => {
            state.repeat_last = Some(record.clone());
            Vec::new()
        }
        Event::Escape { selections } => {
            let effects = vec![Effect::InvalidateHistory];
            let outcome = state.escape(*selections);
            // Delivered Escape: invalidate history, pop, show the exit hint,
            // seal, refocus — regardless of which level was popped. Row 10
            // adds the keep-primary recovery for a multi-member set.
            // FEUX-0004 E-4: Escape clears the count.
            state.clear_count();
            let mut effects = effects;
            if outcome == EscapeOutcome::KeptPrimary {
                effects.push(Effect::KeepPrimary);
            }
            effects.push(Effect::SealTypingGroup);
            effects.push(Effect::FocusEditor);
            state.set_message(ContextState::ESC_HINT, false);
            effects
        }
        Event::OpenMenu(menu) => {
            // FEUX-0004 E-4: opening a menu clears the count.
            state.clear_count();
            state.open_menu(*menu);
            vec![Effect::ClearPreview, Effect::SealTypingGroup]
        }
        Event::CloseMenu => {
            state.close_menu();
            vec![Effect::ClearPreview, Effect::FocusEditor]
        }
        Event::MenuMove(forward) => {
            state.menu_move(*forward);
            vec![Effect::ClearPreview]
        }
        Event::ExternalSelectionChanged => {
            state.external_selection_changed();
            if state.mode.active() {
                vec![Effect::SealTypingGroup]
            } else {
                Vec::new()
            }
        }
        Event::MenuTargetInvalid => {
            state.menu_target_invalid();
            Vec::new()
        }
        Event::Blur => {
            // Blur suspends the mode but cancels the AwaitArgs capture
            // (§5.4.1: suspend retains the mode, cancels to Command; FEUX-0004 E-6).
            state.await_args = None;
            if state.mode.active() {
                vec![Effect::SealTypingGroup]
            } else {
                Vec::new()
            }
        }
        Event::CountDigit(d) => {
            // FEUX-0004 E-4: count state machine
            // - 0 only extends (leading 0 is not a count)
            // - Cap at COUNT_CAP (10,000)
            // - A digit that would exceed the cap refuses with the cap named and clears
            let current = state.count.unwrap_or(0);
            let new_count = if current == 0 && *d == 0 {
                // Leading 0 is not a count - it dispatches as an unbound key
                // notice (E-4), exactly like the host's unbound path.
                state.set_message(ContextState::UNBOUND, false);
                None
            } else {
                let next = current * 10 + d;
                if next > COUNT_CAP {
                    // Cap reached - refuse and clear
                    state.set_message(COUNT_CAP_REFUSAL, true);
                    None
                } else {
                    Some(next)
                }
            };
            state.set_count(new_count);
            Vec::new()
        }
        Event::ClearCount => {
            // FEUX-0004 E-4: a dispatch path outside the statechart consumed
            // the count (native motions, unbound/refused dispatches).
            state.clear_count();
            Vec::new()
        }
    }
}

/// Feed one admitted activation into the burst window (FEUX-0002 E-15,
/// X07). Returns [`Effect::ShowBurstHint`] exactly when the threshold is
/// freshly crossed: ≥5 activations within 1.5 s, ≥4 of them mutating or
/// typing-entry, and no hint in the last 30 s. The window ages by
/// `now_ms`; a session reset (`exit`) clears it with the rest of the state.
#[must_use]
fn burst_effect(
    state: &mut ContextState,
    command: Option<ContextCommand>,
    now_ms: u64,
    enabled: bool,
) -> Vec<Effect> {
    if !enabled {
        return Vec::new();
    }
    let mutating = command.is_none_or(|c| c.mutates() || c.enters_typing());
    state.burst_window.push_back(BurstActivation {
        at_ms: now_ms,
        mutating,
    });
    while state
        .burst_window
        .front()
        .is_some_and(|a| now_ms.saturating_sub(a.at_ms) > BURST_WINDOW_MS)
    {
        state.burst_window.pop_front();
    }
    let total = state.burst_window.len();
    let mutating_count = state.burst_window.iter().filter(|a| a.mutating).count();
    // `last_burst_ms == 0` means "never shown in this session": the first
    // crossing is always eligible (the suppression window only gates
    // re-shows).
    let since_last = if state.last_burst_ms == 0 {
        u64::MAX
    } else {
        now_ms.saturating_sub(state.last_burst_ms)
    };
    if total >= BURST_MIN_ACTIVATIONS
        && mutating_count >= BURST_MIN_MUTATING
        && since_last >= BURST_SUPPRESS_MS
    {
        state.last_burst_ms = now_ms;
        vec![Effect::ShowBurstHint]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    //! The FEUX-0002 §5.4 transition matrix, behavior-locked to the
    //! delivered host semantics (X02 moved them; the host suites assert the
    //! observable behavior unchanged).

    use super::*;

    /// Drive the statechart under the deterministic test clock with the
    /// burst hint disabled (the X07 burst matrix owns the enabled cases).
    fn r(state: &mut ContextState, event: &Event, now_ms: u64, enabled: bool) -> Vec<Effect> {
        reduce(state, event, now_ms, enabled)
    }

    #[test]
    fn activation_starts_command_with_the_intro_hint() {
        let mut state = ContextState::default();
        assert_eq!(r(&mut state, &Event::Activate, 0, false), Vec::new());
        assert_eq!(state.mode(), ContextMode::Command);
        assert_eq!(state.message(), (ContextState::INTRO, false));
        assert!(!state.attention);
        assert_eq!(state.menu(), None);
    }

    #[test]
    fn escape_pops_exactly_one_level_menu_first_then_typing() {
        // Menu open: pops the menu, mode untouched, seal + refocus effects.
        let mut state = ContextState::activated();
        state.open_menu(ContextMenu::Select);
        let effects = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
        assert!(effects.contains(&Effect::InvalidateHistory));
        assert!(effects.contains(&Effect::SealTypingGroup));
        assert!(effects.contains(&Effect::FocusEditor));
        assert_eq!(state.menu(), None);
        assert_eq!(state.mode(), ContextMode::Command);
        assert_eq!(state.message(), (ContextState::ESC_HINT, false));

        // Typing: leaves typing (no menu to pop).
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::EnterTyping, 0, false);
        assert_eq!(r(&mut state, &Event::Escape { selections: 1 }, 0, false), {
            let mut expected = vec![Effect::InvalidateHistory];
            expected.push(Effect::SealTypingGroup);
            expected.push(Effect::FocusEditor);
            expected
        });
        assert_eq!(state.mode(), ContextMode::Command);

        // At Command: stays; the hint is the deliverable.
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
        assert_eq!(state.mode(), ContextMode::Command);
        assert_eq!(state.message(), (ContextState::ESC_HINT, false));
    }

    #[test]
    fn menus_open_at_the_first_row_cycle_with_wraparound_and_close() {
        let mut state = ContextState::activated();
        let effects = r(&mut state, &Event::OpenMenu(ContextMenu::Select), 0, false);
        assert_eq!(effects, vec![Effect::ClearPreview, Effect::SealTypingGroup]);
        assert_eq!(state.menu(), Some(ContextMenu::Select));
        assert_eq!(state.selected(), 0);
        assert_eq!(state.menu_command(), Some(ContextCommand::SelectWord));

        assert_eq!(
            r(&mut state, &Event::MenuMove(true), 0, false),
            vec![Effect::ClearPreview]
        );
        assert_eq!(state.selected(), 1);
        assert_eq!(state.menu_command(), Some(ContextCommand::SelectLine));
        let rows = ContextMenu::Select.commands().len() - 1;
        for _ in 0..rows {
            let _ = r(&mut state, &Event::MenuMove(true), 0, false);
        }
        assert_eq!(state.selected(), 0, "wrap forward lands on the first row");
        assert_eq!(
            r(&mut state, &Event::MenuMove(false), 0, false),
            vec![Effect::ClearPreview]
        );
        assert_eq!(
            state.menu_command(),
            Some(ContextCommand::KeepMatching { keep: false }),
            "wrap backward lands on the last row"
        );

        assert_eq!(
            r(&mut state, &Event::CloseMenu, 0, false),
            vec![Effect::ClearPreview, Effect::FocusEditor,]
        );
        assert_eq!(state.menu(), None);
    }

    #[test]
    fn menu_move_without_a_menu_keeps_the_selection() {
        let mut state = ContextState::activated();
        assert_eq!(
            r(&mut state, &Event::MenuMove(true), 0, false),
            vec![Effect::ClearPreview]
        );
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn admitted_runs_close_menus_and_name_the_command() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::OpenMenu(ContextMenu::More), 0, false);
        let _ = r(
            &mut state,
            &Event::RunAdmitted(ContextCommand::Change),
            0,
            false,
        );
        assert_eq!(state.menu(), None);
        assert_eq!(state.selected(), 0);
        assert_eq!(state.message(), ("Change selection", false));
    }

    #[test]
    fn typing_entries_switch_mode_with_the_typing_hint_and_escape_returns() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::EnterTyping, 0, false);
        assert_eq!(state.mode(), ContextMode::Typing);
        assert_eq!(state.message(), (ContextState::TYPING_HINT, false));
        let _ = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
        assert_eq!(state.mode(), ContextMode::Command);
    }

    #[test]
    fn external_selection_changes_close_the_menu_and_seal_only_while_active() {
        // Active: menu closes, typing group seals.
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::OpenMenu(ContextMenu::Go), 0, false);
        assert_eq!(
            r(&mut state, &Event::ExternalSelectionChanged, 0, false),
            vec![Effect::SealTypingGroup]
        );
        assert_eq!(state.menu(), None);

        // Off: nothing happens.
        let mut state = ContextState::default();
        assert_eq!(
            r(&mut state, &Event::ExternalSelectionChanged, 0, false),
            Vec::new()
        );
        assert_eq!(state.mode(), ContextMode::Off);
    }

    #[test]
    fn stale_menu_targets_close_the_menu_with_the_retype_notice() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::OpenMenu(ContextMenu::More), 0, false);
        assert_eq!(
            r(&mut state, &Event::MenuTargetInvalid, 0, false),
            Vec::new()
        );
        assert_eq!(state.menu(), None);
        assert_eq!(state.message(), (ContextState::MENU_TARGET_STALE, false));
    }

    #[test]
    fn blur_seals_only_while_active_and_retains_the_mode() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::EnterTyping, 0, false);
        assert_eq!(
            r(&mut state, &Event::Blur, 0, false),
            vec![Effect::SealTypingGroup]
        );
        assert_eq!(
            state.mode(),
            ContextMode::Typing,
            "blur never exits the mode"
        );
        let mut state = ContextState::default();
        assert_eq!(r(&mut state, &Event::Blur, 0, false), Vec::new());
    }

    #[test]
    fn exit_resets_to_the_off_default() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::OpenMenu(ContextMenu::Select), 0, false);
        state.set_message("n", true);
        state.exit();
        assert_eq!(state, ContextState::default());
        assert_eq!(state.mode(), ContextMode::Off);
        assert_eq!(state.message(), ("", false));
    }

    #[test]
    fn every_menu_lists_each_command_exactly_once() {
        for menu in [
            ContextMenu::Select,
            ContextMenu::Go,
            ContextMenu::More,
            ContextMenu::Transform,
            ContextMenu::Lines,
            ContextMenu::Surround,
        ] {
            let commands = menu.commands();
            for (index, command) in commands.iter().enumerate() {
                assert!(
                    !commands[..index].contains(command),
                    "{menu:?} duplicates {command:?}"
                );
            }
        }
    }

    #[test]
    fn await_args_captures_and_cancels_correctly() {
        // Enter AwaitArgs for find-char forward
        let mut state = ContextState::activated();
        let _ = r(
            &mut state,
            &Event::EnterAwaitArgs(AwaitArgs::find_char(true)),
            0,
            false,
        );
        assert!(state.await_args().is_some());
        assert_eq!(state.message(), (AWAIT_CHAR_HINT, false));

        // Escape cancels it
        let _ = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
        assert!(state.await_args().is_none());

        // Enter AwaitArgs for surround change (two args)
        let mut state = ContextState::activated();
        let _ = r(
            &mut state,
            &Event::EnterAwaitArgs(AwaitArgs::surround_change()),
            0,
            false,
        );
        assert!(state.await_args().is_some());
        let args = state.await_args().unwrap();
        assert_eq!(args.index, 0);
        assert_eq!(args.command, PendingCommand::SurroundChange);
        assert_eq!(state.message(), (AWAIT_CHAR_HINT, false));

        // Collect first char - should advance to second arg
        let collected = state.collect_char('(');
        assert!(collected.is_none()); // Not complete yet
        let args = state.await_args().unwrap();
        assert_eq!(args.index, 1);
        assert_eq!(args.collected_chars, vec!['(']);
        assert_eq!(state.message(), (AWAIT_SECOND_CHAR_HINT, false));

        // Collect second char - should complete
        let collected = state.collect_char('[');
        assert!(collected.is_some());
        assert_eq!(collected.unwrap(), vec!['(', '[']);
        assert!(state.await_args().is_none());
    }

    #[test]
    fn collect_char_advances_and_completes() {
        // Test single-char command (SurroundAdd)
        let mut state = ContextState::activated();
        let _ = r(
            &mut state,
            &Event::EnterAwaitArgs(AwaitArgs::surround_add()),
            0,
            false,
        );
        assert!(state.await_args().is_some());

        // Collect one char - should complete
        let collected = state.collect_char('(');
        assert!(collected.is_some());
        assert_eq!(collected.unwrap(), vec!['(']);
        assert!(state.await_args().is_none());

        // Test two-char command (SurroundChange)
        let mut state = ContextState::activated();
        let _ = r(
            &mut state,
            &Event::EnterAwaitArgs(AwaitArgs::surround_change()),
            0,
            false,
        );
        assert!(state.await_args().is_some());

        // Collect first char - should advance but not complete
        let collected = state.collect_char('(');
        assert!(collected.is_none());
        assert!(state.await_args().is_some());
        let args = state.await_args().unwrap();
        assert_eq!(args.index, 1);
        assert_eq!(args.collected_chars, vec!['(']);

        // Collect second char - should complete
        let collected = state.collect_char('[');
        assert!(collected.is_some());
        assert_eq!(collected.unwrap(), vec!['(', '[']);
        assert!(state.await_args().is_none());
    }

    // FEUX-0004 X06: Count state machine tests (E-4 §5.2)

    #[test]
    fn count_digit_starts_and_extends_count() {
        // Starting from Command mode with no count
        let mut state = ContextState::activated();
        assert_eq!(state.count(), None);

        // Digit 1 starts the count
        let _ = r(&mut state, &Event::CountDigit(1), 0, false);
        assert_eq!(state.count(), Some(1));

        // Digit 2 extends to 12
        let _ = r(&mut state, &Event::CountDigit(2), 0, false);
        assert_eq!(state.count(), Some(12));

        // Digit 3 extends to 123
        let _ = r(&mut state, &Event::CountDigit(3), 0, false);
        assert_eq!(state.count(), Some(123));
    }

    #[test]
    fn count_digit_zero_only_extends() {
        // Start with a count of 1
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(1), 0, false);
        assert_eq!(state.count(), Some(1));

        // Digit 0 extends to 10
        let _ = r(&mut state, &Event::CountDigit(0), 0, false);
        assert_eq!(state.count(), Some(10));
    }

    #[test]
    fn count_digit_leading_zero_is_not_a_count() {
        // Leading 0 is not a count - should stay None and surface the
        // unbound-key notice (E-4).
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(0), 0, false);
        assert_eq!(state.count(), None);
        assert_eq!(state.message(), (ContextState::UNBOUND, false));
    }

    #[test]
    fn clear_count_event_clears_without_side_effects() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(7), 0, false);
        assert_eq!(state.count(), Some(7));
        let effects = r(&mut state, &Event::ClearCount, 0, false);
        assert!(effects.is_empty());
        assert_eq!(state.count(), None);
        // No message is set by clearing.
        assert_ne!(state.message(), (COUNT_CAP_REFUSAL, true));
    }

    #[test]
    fn count_cap_refuses_and_clears() {
        // Build up to 10000 (cap is 10000, not 1000)
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(1), 0, false);
        let _ = r(&mut state, &Event::CountDigit(0), 0, false);
        let _ = r(&mut state, &Event::CountDigit(0), 0, false);
        let _ = r(&mut state, &Event::CountDigit(0), 0, false);
        let _ = r(&mut state, &Event::CountDigit(0), 0, false);
        assert_eq!(state.count(), Some(10000));

        // One more digit would exceed the cap
        let _ = r(&mut state, &Event::CountDigit(1), 0, false);
        assert_eq!(state.count(), None);
        assert_eq!(state.message(), (COUNT_CAP_REFUSAL, true));
    }

    #[test]
    fn count_clears_on_escape() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(5), 0, false);
        assert_eq!(state.count(), Some(5));

        let _ = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
        assert_eq!(state.count(), None);
    }

    #[test]
    fn count_clears_on_open_menu() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(5), 0, false);
        assert_eq!(state.count(), Some(5));

        let _ = r(&mut state, &Event::OpenMenu(ContextMenu::Select), 0, false);
        assert_eq!(state.count(), None);
    }

    #[test]
    fn count_clears_on_enter_typing() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(5), 0, false);
        assert_eq!(state.count(), Some(5));

        let _ = r(&mut state, &Event::EnterTyping, 0, false);
        assert_eq!(state.count(), None);
    }

    #[test]
    fn count_clears_on_run_admitted() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(5), 0, false);
        assert_eq!(state.count(), Some(5));

        let _ = r(
            &mut state,
            &Event::RunAdmitted(ContextCommand::Delete),
            0,
            false,
        );
        assert_eq!(state.count(), None);
    }

    #[test]
    fn count_retained_through_await_args() {
        let mut state = ContextState::activated();
        let _ = r(&mut state, &Event::CountDigit(5), 0, false);
        assert_eq!(state.count(), Some(5));

        // Enter AwaitArgs - count should be retained
        let _ = r(
            &mut state,
            &Event::EnterAwaitArgs(AwaitArgs::find_char(true)),
            0,
            false,
        );
        assert_eq!(state.count(), Some(5));
        assert!(state.await_args().is_some());
    }

    #[test]
    fn count_methods_work() {
        let mut state = ContextState::activated();

        // Initially no count
        assert_eq!(state.count(), None);

        // Set count
        state.set_count(Some(42));
        assert_eq!(state.count(), Some(42));

        // Clear count
        state.clear_count();
        assert_eq!(state.count(), None);
    }
}
