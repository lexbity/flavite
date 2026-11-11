//! FEUX-0002 X02: the §5.4 transition matrix exercised through the crate's
//! public API — the exact surface the host consumes. The inline unit tests
//! in `session.rs` cover the same decisions at module level; this suite
//! proves the public surface is sufficient for the host contract and that
//! every matrix row is reachable without touching private fields.

use flavite_edit_context::{
    ContextCommand, ContextMenu, ContextMode, ContextState, Effect, Event, reduce,
};

fn activated() -> ContextState {
    let mut state = ContextState::default();
    let _ = reduce(&mut state, &Event::Activate);
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
    let _ = reduce(&mut state, &Event::OpenMenu(ContextMenu::Select));
    let effects = reduce(&mut state, &Event::Escape);
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
    let _ = reduce(&mut state, &Event::EnterTyping);
    let effects = reduce(&mut state, &Event::Escape);
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
    let _ = reduce(&mut state, &Event::Escape);
    assert_eq!(state.mode(), ContextMode::Command);
    assert_eq!(state.message(), (ContextState::ESC_HINT, false));
}

#[test]
fn matrix_menu_rows_open_cycle_and_close_with_delivered_effects() {
    let mut state = activated();
    assert_eq!(
        reduce(&mut state, &Event::OpenMenu(ContextMenu::Type)),
        vec![Effect::ClearPreview, Effect::SealTypingGroup]
    );
    assert_eq!(state.selected(), 0);
    assert_eq!(state.menu_command(), Some(ContextCommand::InsertStart));

    assert_eq!(
        reduce(&mut state, &Event::MenuMove(true)),
        vec![Effect::ClearPreview]
    );
    assert_eq!(state.selected(), 1);
    // Wrap backward from row 1 lands on the last row of the Type menu.
    let _ = reduce(&mut state, &Event::MenuMove(false));
    let _ = reduce(&mut state, &Event::MenuMove(false));
    assert_eq!(state.selected(), 6);
    assert_eq!(state.menu_command(), Some(ContextCommand::Change));

    assert_eq!(
        reduce(&mut state, &Event::CloseMenu),
        vec![Effect::ClearPreview, Effect::FocusEditor]
    );
    assert_eq!(state.menu(), None);
}

#[test]
fn matrix_typing_and_run_rows_seal_and_clear_via_the_host() {
    let mut state = activated();
    let _ = reduce(&mut state, &Event::RunAdmitted(ContextCommand::Delete));
    assert_eq!(state.message(), ("Delete selection", false));
    assert_eq!(state.menu(), None);

    let effects = reduce(&mut state, &Event::EnterTyping);
    assert!(effects.is_empty());
    assert_eq!(state.mode(), ContextMode::Typing);
    assert_eq!(state.message(), (ContextState::TYPING_HINT, false));
}

#[test]
fn matrix_external_and_reconcile_rows_match_delivered_semantics() {
    // External selection while active: menu closes, typing group seals.
    let mut state = activated();
    let _ = reduce(&mut state, &Event::OpenMenu(ContextMenu::Go));
    assert_eq!(
        reduce(&mut state, &Event::ExternalSelectionChanged),
        vec![Effect::SealTypingGroup]
    );
    assert_eq!(state.menu(), None);

    // Off: the row is inert.
    let mut state = ContextState::default();
    assert_eq!(reduce(&mut state, &Event::ExternalSelectionChanged), vec![]);

    // Stale menu target: menu closes with the retype notice.
    let mut state = activated();
    let _ = reduce(&mut state, &Event::OpenMenu(ContextMenu::More));
    assert_eq!(reduce(&mut state, &Event::MenuTargetInvalid), vec![]);
    assert_eq!(state.menu(), None);
    assert_eq!(state.message(), (ContextState::MENU_TARGET_STALE, false));

    // Blur seals while active and never leaves the mode.
    let mut state = activated();
    let _ = reduce(&mut state, &Event::EnterTyping);
    assert_eq!(
        reduce(&mut state, &Event::Blur),
        vec![Effect::SealTypingGroup]
    );
    assert_eq!(state.mode(), ContextMode::Typing);
}
