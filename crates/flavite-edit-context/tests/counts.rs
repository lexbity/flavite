// FEUX-0004 X06: Count state machine integration tests
// Tests the count accumulator behavior in the session state

use flavite_edit_context::{
    session::{ContextState, Event},
    ContextCommand, ContextMenu, reduce,
};

#[test]
fn count_digit_sequence_builds_count() {
    let mut state = ContextState::activated();
    
    // Type 1, 2, 3 -> count should be 123
    let _ = reduce(&mut state, &Event::CountDigit(1), 0, false);
    let _ = reduce(&mut state, &Event::CountDigit(2), 0, false);
    let _ = reduce(&mut state, &Event::CountDigit(3), 0, false);
    
    assert_eq!(state.count(), Some(123));
}

#[test]
fn count_leading_zero_is_ignored() {
    let mut state = ContextState::activated();
    
    // Leading 0 should not start a count
    let _ = reduce(&mut state, &Event::CountDigit(0), 0, false);
    assert_eq!(state.count(), None);
    
    // 0 after a digit should extend the count
    let _ = reduce(&mut state, &Event::CountDigit(1), 0, false);
    let _ = reduce(&mut state, &Event::CountDigit(0), 0, false);
    assert_eq!(state.count(), Some(10));
}

#[test]
fn count_cap_at_10000() {
    let mut state = ContextState::activated();
    
    // Build up to 10000
    let _ = reduce(&mut state, &Event::CountDigit(1), 0, false);
    let _ = reduce(&mut state, &Event::CountDigit(0), 0, false);
    let _ = reduce(&mut state, &Event::CountDigit(0), 0, false);
    let _ = reduce(&mut state, &Event::CountDigit(0), 0, false);
    let _ = reduce(&mut state, &Event::CountDigit(0), 0, false);
    assert_eq!(state.count(), Some(10000));
    
    // One more digit exceeds the cap
    let _ = reduce(&mut state, &Event::CountDigit(1), 0, false);
    assert_eq!(state.count(), None);
}

#[test]
fn count_clears_on_escape() {
    let mut state = ContextState::activated();
    let _ = reduce(&mut state, &Event::CountDigit(5), 0, false);
    assert_eq!(state.count(), Some(5));
    
    let _ = reduce(&mut state, &Event::Escape { selections: 1 }, 0, false);
    assert_eq!(state.count(), None);
}

#[test]
fn count_clears_on_menu_open() {
    let mut state = ContextState::activated();
    let _ = reduce(&mut state, &Event::CountDigit(5), 0, false);
    assert_eq!(state.count(), Some(5));
    
    let _ = reduce(&mut state, &Event::OpenMenu(ContextMenu::Select), 0, false);
    assert_eq!(state.count(), None);
}

#[test]
fn count_clears_on_run_admitted() {
    let mut state = ContextState::activated();
    let _ = reduce(&mut state, &Event::CountDigit(5), 0, false);
    assert_eq!(state.count(), Some(5));
    
    let _ = reduce(&mut state, &Event::RunAdmitted(ContextCommand::Delete), 0, false);
    assert_eq!(state.count(), None);
}

#[test]
fn count_set_and_clear_methods() {
    let mut state = ContextState::activated();
    
    assert_eq!(state.count(), None);
    
    state.set_count(Some(42));
    assert_eq!(state.count(), Some(42));
    
    state.clear_count();
    assert_eq!(state.count(), None);
}
