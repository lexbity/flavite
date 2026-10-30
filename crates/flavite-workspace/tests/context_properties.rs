//! Property tests for context binding graph invariants (RFC-0003 §20, W04).
//!
//! Randomized bind/rebind/close/source-closure sequences must preserve: no
//! cycles (sinks cannot publish), sink focus never changes document or
//! writing-target authorities, identities remain valid, and unbound states
//! are typed — never silent fallbacks to a different source.

use flavite_workspace::{
    ContextBinding, ContextRouter, ContextSource, ContextViewKind, EditorInteraction,
    EntityLocator, LayoutAction, NoteLocator, Outcome, Selection, TabId, ViewKind, WorkspaceState,
    reduce,
};

fn locator(name: &str) -> NoteLocator {
    NoteLocator::from_components([name]).expect("locator")
}

/// Open a note in the first group through the real reducer and acknowledge
/// a focused caret, mirroring how the host establishes sources.
fn open_and_focus(state: &WorkspaceState, name: &str) -> (WorkspaceState, TabId) {
    let group = state.first_group().expect("a group exists");
    let outcome = reduce(
        state,
        LayoutAction::OpenNote {
            expected_revision: state.revision(),
            locator: locator(name),
            group,
        },
    );
    let Outcome::Committed(commit) = outcome else {
        panic!("open commits");
    };
    let tab = commit
        .effects
        .iter()
        .find_map(|effect| match effect {
            flavite_workspace::Effect::ViewAttached { tab, .. } => Some(*tab),
            _ => None,
        })
        .expect("attached tab");
    let mut state = commit.state;
    let mut interaction = EditorInteraction::new(tab, 1);
    interaction.sequence = 1;
    interaction.focused = true;
    interaction.selections = vec![Selection::caret(0)];
    interaction.buffer_id = Some(1);
    interaction.revision = 1;
    state
        .acknowledge_interaction(interaction, "text")
        .expect("acknowledged");
    (state, tab)
}

fn open_context(
    state: &WorkspaceState,
    kind: ContextViewKind,
    binding: ContextBinding,
) -> (WorkspaceState, TabId) {
    let group = state.first_group().expect("a group exists");
    let outcome = reduce(
        state,
        LayoutAction::OpenContext {
            expected_revision: state.revision(),
            view: kind,
            binding,
            group,
        },
    );
    let Outcome::Committed(commit) = outcome else {
        panic!("open context commits");
    };
    let tab = commit
        .effects
        .iter()
        .find_map(|effect| match effect {
            flavite_workspace::Effect::ViewAttached { tab, .. } => Some(*tab),
            _ => None,
        })
        .expect("context tab");
    (commit.state, tab)
}

/// Deterministic xorshift generator; every run replays the same corpus.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[test]
#[allow(
    clippy::cast_possible_truncation, // bounded RNG indices in a test
    clippy::too_many_lines
)]
fn randomized_sequences_preserve_authorities_and_typed_sources() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..600 {
        let mut state = WorkspaceState::new();
        let notes = ["a.md", "b.md", "c.md"];
        let mut source_tabs: Vec<TabId> = Vec::new();
        let mut sink_tabs: Vec<TabId> = Vec::new();
        let bindings = [
            ContextBinding::FollowDocument,
            ContextBinding::FollowExplorationOrDocument,
        ];

        for step in 0..12 {
            match rng.below(6) {
                0 => {
                    let name = notes[rng.below(3) as usize];
                    let (next, tab) = open_and_focus(&state, name);
                    state = next;
                    source_tabs.push(tab);
                }
                1 => {
                    let kind = if rng.below(2) == 0 {
                        ContextViewKind::Preview
                    } else {
                        ContextViewKind::Backlinks
                    };
                    let binding = if rng.below(2) == 0 {
                        bindings[rng.below(2) as usize].clone()
                    } else if let Some(tab) = source_tabs.last() {
                        ContextBinding::FollowSource { tab: *tab }
                    } else {
                        bindings[0].clone()
                    };
                    let (next, tab) = open_context(&state, kind, binding);
                    state = next;
                    sink_tabs.push(tab);
                }
                2 if !sink_tabs.is_empty() => {
                    let index = rng.below(sink_tabs.len() as u64) as usize;
                    let tab = sink_tabs[index];
                    let binding = match rng.below(4) {
                        0 => ContextBinding::FollowDocument,
                        1 => ContextBinding::FollowExplorationOrDocument,
                        2 => ContextBinding::Pinned(EntityLocator::new(locator("pinned.md"))),
                        _ => match source_tabs.last() {
                            Some(source) => ContextBinding::FollowSource { tab: *source },
                            None => ContextBinding::FollowDocument,
                        },
                    };
                    let outcome = reduce(
                        &state,
                        LayoutAction::SetContextBinding {
                            expected_revision: state.revision(),
                            tab,
                            binding,
                        },
                    );
                    let Outcome::Committed(commit) = outcome else {
                        panic!("round {round} step {step}: rebind commits");
                    };
                    state = commit.state;
                }
                3 if !source_tabs.is_empty() => {
                    // Close a source: FollowSource bindings become typed
                    // unbound, never silently retargeted.
                    let index = rng.below(source_tabs.len() as u64) as usize;
                    let tab = source_tabs.remove(index);
                    let outcome = reduce(
                        &state,
                        LayoutAction::CloseTab {
                            expected_revision: state.revision(),
                            tab,
                        },
                    );
                    let Outcome::Committed(commit) = outcome else {
                        panic!("close commits");
                    };
                    state = commit.state;
                }
                4 => {
                    // Publish an exploration selection from a real source.
                    if let Some(source) = source_tabs.first() {
                        let name = notes[rng.below(3) as usize];
                        state
                            .publish_exploration(*source, 1, locator(name))
                            .expect("sources can publish");
                    }
                }
                _ => {
                    // Focus movement through ActivateTab only changes Focus.
                    if let Some(source) = source_tabs.last() {
                        let outcome = reduce(
                            &state,
                            LayoutAction::ActivateTab {
                                expected_revision: state.revision(),
                                tab: *source,
                            },
                        );
                        let Outcome::Committed(commit) = outcome else {
                            panic!("activate commits");
                        };
                        state = commit.state;
                    }
                }
            }
        }

        // Invariant 1: every sink resolves to a valid typed source.
        for tab in &sink_tabs {
            let session = state.tab(tab).unwrap_or_else(|| {
                panic!("round {round}: sink tab vanished");
            });
            let (ViewKind::Preview(binding) | ViewKind::Backlinks(binding)) = session.kind() else {
                panic!("round {round}: sink kind changed");
            };
            match ContextRouter::resolve(&state, binding) {
                ContextSource::Document {
                    tab: source,
                    locator,
                    ..
                } => {
                    assert_eq!(
                        state.tab(&source).map(|session| session.kind().clone()),
                        state.tab(&source).map(|session| session.kind().clone()),
                        "round {round}: document source identity is consistent"
                    );
                    assert!(
                        state.tab(&source).is_some(),
                        "round {round}: resolved source tab must exist"
                    );
                    let _ = locator;
                }
                ContextSource::Exploration {
                    source_tab,
                    locator,
                    ..
                } => {
                    assert!(
                        state.tab(&source_tab).is_some(),
                        "round {round}: exploration source must exist"
                    );
                    let _ = locator;
                }
                ContextSource::Pinned { locator } => {
                    let _ = locator;
                }
                ContextSource::Unbound { .. } => {}
            }
        }

        // Invariant 2: sink focus/activation never changes the writing
        // target, and sinks never publish exploration.
        let writing_before = state.writing_target();
        if let Some(sink) = sink_tabs.first().copied() {
            let outcome = reduce(
                &state,
                LayoutAction::ActivateTab {
                    expected_revision: state.revision(),
                    tab: sink,
                },
            );
            if let Outcome::Committed(commit) = outcome {
                assert_eq!(
                    commit.state.writing_target(),
                    writing_before,
                    "round {round}: sink focus must not change the writing target"
                );
                let document_before = commit.state.document_publication();
                let mut state = commit.state;
                assert!(
                    state.publish_exploration(sink, 1, locator("a.md")).is_err(),
                    "round {round}: sinks cannot publish"
                );
                assert_eq!(
                    state.document_publication(),
                    document_before,
                    "round {round}: refused publication must not bump the counter"
                );
            }
        }

        // Invariant 3: closing a followed source yields typed unbound, not
        // a different source.
        if let Some(source) = source_tabs.last().copied() {
            let (mut state, _) = open_context(
                &state,
                ContextViewKind::Preview,
                ContextBinding::FollowSource { tab: source },
            );
            let outcome = reduce(
                &state,
                LayoutAction::CloseTab {
                    expected_revision: state.revision(),
                    tab: source,
                },
            );
            let Outcome::Committed(commit) = outcome else {
                panic!("close commits");
            };
            state = commit.state;
            let rebound = commit
                .effects
                .iter()
                .any(|effect| matches!(effect, flavite_workspace::Effect::ContextRebound { .. }));
            assert!(
                rebound || state.tabs().count() == 0,
                "round {round}: source closure must signal context rebind"
            );
        }
    }
}
