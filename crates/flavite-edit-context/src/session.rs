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

/// The pure session state of one editor's Context Mode. Never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextState {
    mode: ContextMode,
    menu: Option<ContextMenu>,
    selected: usize,
    /// The pending find-char capture; `None` outside `AwaitChar`. A sub-state
    /// of Command, not a mode of its own (E-8: exactly one new state).
    await_char: Option<AwaitChar>,
    message: String,
    attention: bool,
    /// The sliding activation window driving the burst hint (X07, E-15).
    /// Not a mode; it records admitted activations only.
    burst_window: VecDeque<BurstActivation>,
    /// The last time a burst hint was emitted, in the same clock as
    /// `now_ms` (suppression window).
    last_burst_ms: u64,
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

/// The pending find-char request while the statechart waits for one
/// produced character (FEUX-0002 E-8, §5.4.1). The host resolves it from
/// the next key event carrying produced text; nothing times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitChar {
    /// `f` searches forward from each head; `Shift+F` backward.
    pub forward: bool,
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
    /// An admitted find-char command begins the `AwaitChar` capture (§5.4.1).
    EnterAwaitChar(AwaitChar),
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

    pub const INTRO: &str = "Select text, then act · i types · Esc returns to commands";
    pub const TYPING_HINT: &str = "Typing · click or move to reposition · Esc returns to commands";
    pub const ESC_HINT: &str = "Commands · exit with ✕ or the toggle shortcut";
    pub const UNBOUND: &str = "No command on this key · choose a menu or Type";
    pub const AWAIT_CHAR_HINT: &str = "Waiting for character…";
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

    /// The pending find-char capture, if the statechart is in `AwaitChar`.
    #[must_use]
    pub const fn await_char(&self) -> Option<AwaitChar> {
        self.await_char
    }

    /// Enter `AwaitChar` (§5.4.1 row 5): the state row shows the capture hint.
    pub fn begin_await_char(&mut self, request: AwaitChar) {
        debug_assert!(
            self.mode.active(),
            "AwaitChar entry requires an active mode"
        );
        self.await_char = Some(request);
        self.message = Self::AWAIT_CHAR_HINT.into();
        self.attention = false;
    }

    /// Resolve or cancel the `AwaitChar` capture; returns whether one was
    /// pending.
    pub fn end_await_char(&mut self) -> bool {
        self.await_char.take().is_some()
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

    /// Escape pops exactly one level: a menu first, then typing, and at
    /// Command it stays — keeping the primary when the set has more than
    /// one member (§5.4 row 10: the multi-selection recovery). The host
    /// shows the exit hint in every Command outcome.
    pub fn escape(&mut self, selections: usize) -> EscapeOutcome {
        if self.menu.take().is_some() {
            EscapeOutcome::PoppedMenu
        } else if self.await_char.take().is_some() {
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
        self.await_char = None;
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
            state.admit_run(*command);
            burst_effect(state, Some(*command), now_ms, burst_enabled)
        }
        Event::EnterTyping => {
            state.begin_typing();
            // Every typing entry arrives through an admitted Run first (the
            // host contract), so the activation was already recorded there;
            // recording it again would double-count one physical action.
            Vec::new()
        }
        Event::EnterAwaitChar(request) => {
            state.begin_await_char(*request);
            Vec::new()
        }
        Event::Escape { selections } => {
            let effects = vec![Effect::InvalidateHistory];
            let outcome = state.escape(*selections);
            // Delivered Escape: invalidate history, pop, show the exit hint,
            // seal, refocus — regardless of which level was popped. Row 10
            // adds the keep-primary recovery for a multi-member set.
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
            // Blur suspends the mode but cancels the AwaitChar capture
            // (§5.4.1: suspend retains the mode, cancels to Command).
            state.await_char = None;
            if state.mode.active() {
                vec![Effect::SealTypingGroup]
            } else {
                Vec::new()
            }
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
            Some(ContextCommand::SplitSelection),
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
        let _ = r(&mut state, &Event::OpenMenu(ContextMenu::Type), 0, false);
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
            ContextMenu::Type,
            ContextMenu::More,
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
}
