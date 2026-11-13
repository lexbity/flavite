//! FEUX-0002 X02: the §5.4 transition matrix exercised through the crate's
//! public API — the exact surface the host consumes. The inline unit tests
//! in `session.rs` cover the same decisions at module level; this suite
//! proves the public surface is sufficient for the host contract and that
//! every matrix row is reachable without touching private fields.

use flavite_edit_context::{
    ContextCommand, ContextMenu, ContextMode, ContextState, Effect, Event, reduce,
};

/// Drive the public statechart with the deterministic test clock and the
/// burst hint disabled (the burst matrix has its own suite).
fn r(state: &mut ContextState, event: &Event, now_ms: u64, enabled: bool) -> Vec<Effect> {
    reduce(state, event, now_ms, enabled)
}

fn activated() -> ContextState {
    let mut state = ContextState::default();
    let _ = r(&mut state, &Event::Activate, 0, false);
    state
}

#[test]
fn activate_creates_the_delivered_command_state() {
    let state = activated();
    assert_eq!(state.mode(), ContextMode::Command);
    assert_eq!(state.message(), (ContextState::INTRO, false));
    assert_eq!(state.menu(), None);
}

#[test]
fn matrix_escape_row_pops_menu_then_typing_then_stays() {
    // Menu open (delivered: pop the menu, mode untouched).
    let mut state = activated();
    let _ = r(&mut state, &Event::OpenMenu(ContextMenu::Select), 0, false);
    let effects = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
    assert_eq!(
        effects,
        vec![
            Effect::InvalidateHistory,
            Effect::SealTypingGroup,
            Effect::FocusEditor
        ]
    );
    assert_eq!(state.menu(), None);
    assert_eq!(state.mode(), ContextMode::Command);

    // Typing (delivered: leave typing, same effect list).
    let mut state = activated();
    let _ = r(&mut state, &Event::EnterTyping, 0, false);
    let effects = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
    assert_eq!(
        effects,
        vec![
            Effect::InvalidateHistory,
            Effect::SealTypingGroup,
            Effect::FocusEditor
        ]
    );
    assert_eq!(state.mode(), ContextMode::Command);

    // Command (delivered: stay; hint is the deliverable).
    let mut state = activated();
    let _ = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
    assert_eq!(state.mode(), ContextMode::Command);
    assert_eq!(state.message(), (ContextState::ESC_HINT, false));
}

#[test]
fn matrix_menu_rows_open_cycle_and_close_with_delivered_effects() {
    let mut state = activated();
    assert_eq!(
        r(&mut state, &Event::OpenMenu(ContextMenu::Type), 0, false),
        vec![Effect::ClearPreview, Effect::SealTypingGroup]
    );
    assert_eq!(state.selected(), 0);
    assert_eq!(state.menu_command(), Some(ContextCommand::InsertStart));

    assert_eq!(
        r(&mut state, &Event::MenuMove(true), 0, false),
        vec![Effect::ClearPreview]
    );
    assert_eq!(state.selected(), 1);
    // Wrap backward from row 1 lands on the last row of the Type menu.
    let _ = r(&mut state, &Event::MenuMove(false), 0, false);
    let _ = r(&mut state, &Event::MenuMove(false), 0, false);
    assert_eq!(state.selected(), 6);
    assert_eq!(state.menu_command(), Some(ContextCommand::Change));

    assert_eq!(
        r(&mut state, &Event::CloseMenu, 0, false),
        vec![Effect::ClearPreview, Effect::FocusEditor]
    );
    assert_eq!(state.menu(), None);
}

#[test]
fn matrix_typing_and_run_rows_seal_and_clear_via_the_host() {
    let mut state = activated();
    let _ = r(
        &mut state,
        &Event::RunAdmitted(ContextCommand::Delete),
        0,
        false,
    );
    assert_eq!(state.message(), ("Delete selection", false));
    assert_eq!(state.menu(), None);

    let effects = r(&mut state, &Event::EnterTyping, 0, false);
    assert!(effects.is_empty());
    assert_eq!(state.mode(), ContextMode::Typing);
    assert_eq!(state.message(), (ContextState::TYPING_HINT, false));
}

#[test]
fn matrix_await_char_rows_enter_resolve_cancel_and_blur() {
    // Entry (row 5): from Command on FindChar keys the state shows the
    // capture hint and no effects fire.
    let mut state = activated();
    assert_eq!(
        r(
            &mut state,
            &Event::EnterAwaitChar(flavite_edit_context::AwaitChar { forward: true }),
            0,
            false,
        ),
        vec![]
    );
    assert!(state.await_char().is_some());
    assert_eq!(state.message(), (ContextState::AWAIT_CHAR_HINT, false));
    assert_eq!(state.mode(), ContextMode::Command);

    // A produced character resolves (row 6) via the host: ending the
    // capture returns it and the message returns to the find command.
    assert!(state.end_await_char());
    assert!(state.await_char().is_none());

    // Esc cancels (row 7): the capture is cleared, the mode stays, and
    // the delivered Escape effects fire.
    let mut state = activated();
    let _ = r(
        &mut state,
        &Event::EnterAwaitChar(flavite_edit_context::AwaitChar { forward: false }),
        0,
        false,
    );
    let effects = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
    assert!(effects.contains(&Effect::FocusEditor));
    assert!(state.await_char().is_none());
    assert_eq!(state.mode(), ContextMode::Command);

    // Blur cancels to Command while retaining the mode (row 13 + §5.4.1).
    let mut state = activated();
    let _ = r(
        &mut state,
        &Event::EnterAwaitChar(flavite_edit_context::AwaitChar { forward: true }),
        0,
        false,
    );
    let effects = r(&mut state, &Event::Blur, 0, false);
    assert!(state.await_char().is_none());
    assert_eq!(state.mode(), ContextMode::Command);
    assert_eq!(effects, vec![Effect::SealTypingGroup]);

    // An admitted run cancels any pending capture (menu interplay).
    let mut state = activated();
    let _ = r(
        &mut state,
        &Event::EnterAwaitChar(flavite_edit_context::AwaitChar { forward: true }),
        0,
        false,
    );
    let _ = r(
        &mut state,
        &Event::RunAdmitted(ContextCommand::Delete),
        0,
        false,
    );
    assert!(state.await_char().is_none());
}

#[test]
fn matrix_external_and_reconcile_rows_match_delivered_semantics() {
    // External selection while active: menu closes, typing group seals.
    let mut state = activated();
    let _ = r(&mut state, &Event::OpenMenu(ContextMenu::Go), 0, false);
    assert_eq!(
        r(&mut state, &Event::ExternalSelectionChanged, 0, false),
        vec![Effect::SealTypingGroup]
    );
    assert_eq!(state.menu(), None);

    // Off: the row is inert.
    let mut state = ContextState::default();
    assert_eq!(
        r(&mut state, &Event::ExternalSelectionChanged, 0, false),
        vec![]
    );

    // Stale menu target: menu closes with the retype notice.
    let mut state = activated();
    let _ = r(&mut state, &Event::OpenMenu(ContextMenu::More), 0, false);
    assert_eq!(r(&mut state, &Event::MenuTargetInvalid, 0, false), vec![]);
    assert_eq!(state.menu(), None);
    assert_eq!(state.message(), (ContextState::MENU_TARGET_STALE, false));

    // Blur seals while active and never leaves the mode.
    let mut state = activated();
    let _ = r(&mut state, &Event::EnterTyping, 0, false);
    assert_eq!(
        r(&mut state, &Event::Blur, 0, false),
        vec![Effect::SealTypingGroup]
    );
    assert_eq!(state.mode(), ContextMode::Typing);
}

#[test]
fn command_escape_with_multiple_members_keeps_the_primary() {
    // §5.4 row 10: Command Escape with N>1 keeps the primary (the
    // multi-selection recovery) and stays in Command; the host interprets
    // `Effect::KeepPrimary` as `editor.keep_primary`.
    let mut state = activated();
    let effects = r(&mut state, &Event::Escape { selections: 2 }, 0, false);
    assert_eq!(
        effects,
        vec![
            Effect::InvalidateHistory,
            Effect::KeepPrimary,
            Effect::SealTypingGroup,
            Effect::FocusEditor
        ]
    );
    assert_eq!(state.mode(), ContextMode::Command);
    assert_eq!(state.menu(), None);

    // A single member keeps the delivered behavior (no keep-primary).
    let mut state = activated();
    let effects = r(&mut state, &Event::Escape { selections: 1 }, 0, false);
    assert!(!effects.contains(&Effect::KeepPrimary));

    // Typing Escape never reduces the set (row 9), whatever N is.
    let mut state = activated();
    let _ = r(&mut state, &Event::EnterTyping, 0, false);
    let effects = r(&mut state, &Event::Escape { selections: 3 }, 0, false);
    assert!(!effects.contains(&Effect::KeepPrimary));
    assert_eq!(state.mode(), ContextMode::Command);

    // A menu Escape pops the menu only; keep-primary is not emitted.
    let mut state = activated();
    let _ = r(&mut state, &Event::OpenMenu(ContextMenu::Select), 0, false);
    let effects = r(&mut state, &Event::Escape { selections: 2 }, 0, false);
    assert!(!effects.contains(&Effect::KeepPrimary));
}
