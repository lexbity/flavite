//! Modes and structure tools obey the same identity/context reducer contracts.
use flavite_workspace::*;
fn apply(state: &mut WorkspaceState, action: LayoutAction) {
    let Outcome::Committed(commit) = reduce(state, action) else {
        panic!("action rejected")
    };
    *state = commit.state;
}
fn opened() -> WorkspaceState {
    let mut state = WorkspaceState::new();
    let group = state.first_group().unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenNote {
            expected_revision: revision,
            locator: NoteLocator::from_components(["a.md"]).unwrap(),
            group,
        },
    );
    let tab = state.focused_tab();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::NoteLoadReady {
            expected_revision: revision,
            tab,
        },
    );
    state
}
#[test]
fn ready_and_activated_notes_publish_document_anchor_without_writing_authority() {
    let state = opened();
    assert_eq!(state.document_anchor(), Some(state.focused_tab()));
    assert!(state.writing_target().is_none());
    assert!(matches!(
        ContextRouter::resolve(&state, &ContextBinding::FollowDocument),
        ContextSource::Document { .. }
    ));
}
#[test]
fn mode_change_preserves_identity_selection_anchor_and_has_no_implicit_target() {
    let mut state = opened();
    let tab = state.focused_tab();
    let mut input = state.tab(&tab).unwrap().interaction().clone();
    input.buffer_id = Some(7);
    input.revision = 3;
    input.sequence = 1;
    input.focused = true;
    input.selections = vec![CommittedSelection::caret(2)];
    input.viewport_anchor = 1;
    state.acknowledge_interaction(input, "abc").unwrap();
    assert!(state.writing_target().is_some());
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SetNoteMode {
            expected_revision: revision,
            tab,
            mode: NoteMode::Read,
        },
    );
    assert!(state.writing_target().is_none());
    assert!(!state.tab(&tab).unwrap().interaction().focused);
    assert_eq!(
        state.tab(&tab).unwrap().interaction().selections,
        [CommittedSelection::caret(2)]
    );
    assert_eq!(state.tab(&tab).unwrap().interaction().viewport_anchor, 1);
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::DuplicateTab {
            expected_revision: revision,
            tab,
        },
    );
    let duplicate = state.focused_tab();
    assert_ne!(duplicate, tab);
    assert!(
        matches!(state.tab(&duplicate).unwrap().kind(),ViewKind::Note(n) if n.mode()==NoteMode::Read)
    );
    let saved = persist_workspace(&state, "vault");
    let bytes = encode_workspace(&saved).unwrap();
    let decoded = decode_workspace(&bytes, "vault").unwrap();
    assert_eq!(decoded, saved);
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SetNoteMode {
            expected_revision: revision,
            tab,
            mode: NoteMode::Edit,
        },
    );
    assert!(state.writing_target().is_none());
    assert!(
        matches!(state.tab(&duplicate).unwrap().kind(),ViewKind::Note(n) if n.mode()==NoteMode::Read)
    );
}
#[test]
fn composition_and_sink_follow_refusals_leave_state_identical() {
    let mut state = opened();
    let tab = state.focused_tab();
    let mut input = state.tab(&tab).unwrap().interaction().clone();
    input.buffer_id = Some(7);
    input.revision = 1;
    input.sequence = 1;
    input.composition = CompositionState::Active;
    state.acknowledge_interaction(input, "text").unwrap();
    let before = state.clone();
    assert!(matches!(
        reduce(
            &state,
            LayoutAction::SetNoteMode {
                expected_revision: state.revision(),
                tab,
                mode: NoteMode::Read
            }
        ),
        Outcome::Rejected(_)
    ));
    assert_eq!(state, before);
    let mut state = opened();
    let group = state.first_group().unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenContext {
            expected_revision: revision,
            view: ContextViewKind::Outline,
            binding: ContextBinding::FollowDocument,
            group,
        },
    );
    let sink = state.focused_tab();
    let before = state.clone();
    assert!(matches!(
        reduce(
            &state,
            LayoutAction::SetContextBinding {
                expected_revision: state.revision(),
                tab: sink,
                binding: ContextBinding::FollowSource { tab: sink }
            }
        ),
        Outcome::Rejected(_)
    ));
    assert_eq!(state, before);
}

#[test]
fn scrolling_keeps_exact_insertion_authority_but_find_selection_invalidates_it() {
    let mut state = opened();
    let tab = state.focused_tab();
    let mut input = state.tab(&tab).unwrap().interaction().clone();
    input.sequence = 1;
    input.buffer_id = Some(9);
    input.revision = 2;
    input.focused = true;
    input.selections = vec![CommittedSelection::caret(1)];
    state.acknowledge_interaction(input, "a\nb\nc").unwrap();
    let target = state.writing_target().unwrap();
    state.set_content_anchor(tab, 4, "a\nb\nc").unwrap();
    let scrolled = state.writing_target().unwrap();
    assert_eq!(scrolled.caret_text_byte, target.caret_text_byte);
    assert_eq!(scrolled.revision, target.revision);
    assert_eq!(state.tab(&tab).unwrap().interaction().viewport_anchor, 4);
    state
        .set_display_selection(tab, 2, 3, 2, "a\nb\nc")
        .unwrap();
    assert!(state.writing_target().is_none());
}

#[test]
fn open_settings_opens_focuses_and_duplicates_like_every_tool_tab() {
    let mut state = WorkspaceState::new();
    let group = state.first_group().unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenSettings {
            expected_revision: revision,
            group,
        },
    );
    // The fresh workspace opens with one Empty tab; OpenSettings appends
    // and focuses the Settings tab.
    assert_eq!(state.tabs().count(), 2);
    assert!(matches!(
        state.tab(&state.focused_tab()).unwrap().kind(),
        ViewKind::Settings
    ));

    // The tab duplicates (it is stateless; the duplicate is independent).
    let tab = state.focused_tab();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::DuplicateTab {
            expected_revision: revision,
            tab,
        },
    );
    assert_eq!(state.tabs().count(), 3);
    assert!(matches!(
        state.tab(&state.focused_tab()).unwrap().kind(),
        ViewKind::Settings
    ));

    // Closing the original keeps the group alive with the duplicate.
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::CloseTab {
            expected_revision: revision,
            tab,
        },
    );
    assert_eq!(state.tabs().count(), 2);
    assert!(matches!(
        state.tab(&state.focused_tab()).unwrap().kind(),
        ViewKind::Settings
    ));
}
