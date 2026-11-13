//! FEUX-0002 X07: the burst-hint matrix (E-15). Pure statechart surface:
//! the host forwards the resolved `editor.burst_hint_enabled` setting and
//! a monotonic millisecond clock; the threshold, window, and 30 s
//! suppression are all verified here with the deterministic clock.

use flavite_edit_context::{
    BURST_MIN_ACTIVATIONS, BURST_SUPPRESS_MS, BURST_WINDOW_MS, ContextCommand, ContextState,
    Effect, Event, reduce,
};

/// The clock origin: far enough from zero that saturating arithmetic never
/// participates in window math.
const NOW: u64 = 100_000;

fn run(state: &mut ContextState, command: ContextCommand, at: u64, enabled: bool) -> Vec<Effect> {
    reduce(state, &Event::RunAdmitted(command), at, enabled)
}

fn delete(state: &mut ContextState, at: u64) -> Vec<Effect> {
    run(state, ContextCommand::Delete, at, true)
}

#[test]
fn four_activations_in_the_window_never_hint() {
    let mut state = ContextState::activated();
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 - 1 {
        assert_eq!(delete(&mut state, NOW + i * 10), Vec::new());
    }
}

#[test]
fn five_with_four_mutating_hints_exactly_once() {
    let mut state = ContextState::activated();
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        let effects = delete(&mut state, NOW + i * 10);
        if i == BURST_MIN_ACTIVATIONS as u64 - 1 {
            assert_eq!(effects, vec![Effect::ShowBurstHint]);
        } else {
            assert_eq!(effects, Vec::new());
        }
    }
    // The same window does not re-emit on the next activation.
    assert_eq!(delete(&mut state, NOW + 60), Vec::new());
}

#[test]
fn selection_only_activations_do_not_satisfy_the_mutating_floor() {
    // Five activations, but only two are mutating: no hint.
    let mut state = ContextState::activated();
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        let command = match i % 3 {
            0 => ContextCommand::SelectWord,
            1 => ContextCommand::RotatePrimary,
            _ => ContextCommand::Delete,
        };
        let effects = run(&mut state, command, NOW + i * 10, true);
        assert!(effects.is_empty(), "no hint below the mutating floor");
    }
}

#[test]
fn typing_entries_count_as_mutating_through_their_admitted_run() {
    // Four deletes plus one typing-entry command (which does not mutate by
    // itself but enters typing) crosses the floor.
    let mut state = ContextState::activated();
    for i in 0..4 {
        assert!(delete(&mut state, NOW + i * 10).is_empty());
    }
    let effects = run(&mut state, ContextCommand::InsertStart, NOW + 40, true);
    assert_eq!(effects, vec![Effect::ShowBurstHint]);
}

#[test]
fn aged_activations_fall_out_of_the_window() {
    // Five activations spread wider than the window: never a burst.
    let mut state = ContextState::activated();
    let spacing = BURST_WINDOW_MS / 4 + 1;
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        let effects = delete(&mut state, NOW + i * spacing);
        assert!(effects.is_empty(), "spread activations are not a burst");
    }
}

#[test]
fn hint_is_suppressed_for_thirty_seconds() {
    let mut state = ContextState::activated();
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        let _ = delete(&mut state, NOW + i * 10);
    }
    // Re-fire inside the suppression window: window keeps counting, no hint.
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        let effects = delete(&mut state, NOW + 1000 + i * 10);
        assert!(effects.is_empty(), "suppressed inside the 30 s window");
    }
    // After the suppression window, a fresh burst hints again.
    let later = NOW + 1000 + BURST_SUPPRESS_MS + 1;
    let mut effects = Vec::new();
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        effects = delete(&mut state, later + i * 10);
    }
    assert_eq!(effects, vec![Effect::ShowBurstHint]);
}

#[test]
fn disabled_setting_suppresses_everything() {
    let mut state = ContextState::activated();
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        let effects = run(&mut state, ContextCommand::Delete, NOW + i * 10, false);
        assert!(effects.is_empty(), "the setting disables the hint");
    }
}

#[test]
fn mode_exit_resets_the_burst_window() {
    let mut state = ContextState::activated();
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 {
        let _ = delete(&mut state, NOW + i * 10);
    }
    state.exit();
    // A fresh session must accumulate a new window before hinting.
    for i in 0u64..BURST_MIN_ACTIVATIONS as u64 - 1 {
        assert!(delete(&mut state, NOW + 10_000 + i * 10).is_empty());
    }
}
