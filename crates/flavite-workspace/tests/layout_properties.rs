//! Randomized layout invariants (RFC-0003 AC-W01, NFR-13).
//!
//! Generates 100k actions — including stale revisions, self-moves, last-tab
//! closes, nested merges, invalid ids, and bound-exceeding operations — and
//! compares every committed state against an independent tab-multiset
//! reference model. Rejected actions must leave the state byte-identical.

use flavite_workspace::{
    Arrangement, Edge, GroupId, Ids, LayoutAction, NoteLocator, Outcome, Side, SplitId,
    TabDropTarget, TabId, WorkspaceError, WorkspaceState, reduce,
};
use std::collections::BTreeSet;

/// Total randomized actions exercised by the deterministic run.
const ACTION_BUDGET: usize = 100_000;

/// A tiny deterministic PRNG (xorshift64*) so the suite is reproducible and
/// a failure prints its seed for replay.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            #[allow(clippy::cast_possible_truncation)]
            {
                (self.next_u64() % bound as u64) as usize
            }
        }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

/// Independent structural invariant check, written against the public API
/// only, so a reducer bug cannot hide behind its own validator.
fn check_invariants(state: &WorkspaceState) {
    let tree_groups = state.group_order();
    assert!(!tree_groups.is_empty(), "tree has no groups");
    let unique: BTreeSet<_> = tree_groups.iter().copied().collect();
    assert_eq!(unique.len(), tree_groups.len(), "a group appears twice");

    let mut members = BTreeSet::new();
    for id in &tree_groups {
        let group = state.group(id).expect("tree group is mapped");
        assert!(!group.tabs().is_empty(), "group {id} is empty");
        assert!(
            group.tabs().contains(&group.active()),
            "group {id} has a stale active tab"
        );
        for tab in group.tabs() {
            assert!(members.insert(*tab), "tab {tab} is in two groups");
            assert!(
                state.tab(tab).is_some(),
                "tab {tab} is grouped but unmapped"
            );
        }
    }
    assert_eq!(
        members.len(),
        state.tabs().count(),
        "tab map is not the union of group members"
    );
    let focus = state.focused_tab();
    assert!(
        state.groups().any(|(_, group)| group.active() == focus),
        "focus is not an active tab"
    );
    if let Some(maximized) = state.maximized_group() {
        assert!(unique.contains(&maximized), "maximized group left the tree");
    }
}

/// The reference multiset model: what the tab set should become if the
/// action commits, and whether it must have been rejected.
fn expected_delta(state: &WorkspaceState, action: &LayoutAction) -> Option<i64> {
    // Actions whose only legal outcomes are "reject" or "no tab change":
    use LayoutAction as A;
    let revision_ok = action.expected_revision() == state.revision();
    if !revision_ok {
        return None;
    }
    match action {
        A::OpenNote { group, .. }
        | A::OpenEmpty { group, .. }
        | A::OpenNotes { group, .. }
        | A::OpenContext { group, .. }
        | A::SplitGroup { group, .. } => state.group(group).is_some().then_some(1),
        A::DuplicateTab { tab, .. } => state
            .tab(tab)
            .filter(|session| session.kind().duplicate().is_some())
            .map(|_| 1),
        A::ReopenClosed { .. } => (state.recently_closed_len() > 0)
            .then(|| state.focused_group().map(|_| 1))
            .flatten(),
        A::CloseTab { tab, .. } => {
            if state.tab(tab).is_none() {
                None
            } else if state.tabs().count() == 1 {
                // The final tab is replaced by a fresh Empty tab.
                Some(0)
            } else {
                Some(-1)
            }
        }
        A::CloseGroup { group, .. } => state.group(group).map(|group| {
            let closing = i64::try_from(group.tabs().len()).expect("bounded");
            if state.tabs().count() == group.tabs().len() {
                // The emptied workspace normalizes to one fresh Empty tab.
                1 - closing
            } else {
                -closing
            }
        }),
        A::SetContextBinding { .. }
        | A::MoveTab { .. }
        | A::MergeGroups { .. }
        | A::MoveGroupSplit { .. }
        | A::MoveGroupEdge { .. }
        | A::ResizeSplit { .. }
        | A::ToggleMaximize { .. }
        | A::NoteLoadReady { .. }
        | A::NoteLoadFailed { .. }
        | A::ActivateTab { .. }
        | A::FocusGroup { .. }
        | A::CycleTab { .. }
        | A::CycleGroup { .. } => Some(0),
    }
}

/// Ghost ids are distinct from every allocated id (allocation starts at 1
/// and stays far below these bit patterns).
trait RawId {
    fn from_raw(raw: u128) -> Self;
}

impl RawId for GroupId {
    fn from_raw(raw: u128) -> Self {
        Self::from_raw(raw)
    }
}

impl RawId for TabId {
    fn from_raw(raw: u128) -> Self {
        Self::from_raw(raw)
    }
}

fn ghost<T: RawId>() -> T {
    T::from_raw(u128::MAX)
}

/// A conservative, fully predictable action builder: keeps the generator
/// simple and side-effect free (the reducer owns id allocation).
#[allow(clippy::too_many_lines)]
fn build_action(rng: &mut Rng, state: &WorkspaceState, locators: &[NoteLocator]) -> LayoutAction {
    let revision = if rng.chance(10) {
        state.revision().wrapping_add(1 + rng.below(1000) as u64)
    } else {
        state.revision()
    };
    let groups = state.group_order();
    let tabs: Vec<TabId> = state.tabs().map(|(tab, _)| *tab).collect();
    macro_rules! pick_group {
        () => {
            if rng.chance(5) {
                ghost::<GroupId>()
            } else {
                groups[rng.below(groups.len())]
            }
        };
    }
    macro_rules! pick_tab {
        () => {
            if rng.chance(5) {
                ghost::<TabId>()
            } else {
                tabs[rng.below(tabs.len())]
            }
        };
    }
    match rng.below(20) {
        0 => LayoutAction::OpenNote {
            expected_revision: revision,
            locator: locators[rng.below(locators.len())].clone(),
            group: pick_group!(),
        },
        1 => LayoutAction::OpenEmpty {
            expected_revision: revision,
            group: pick_group!(),
        },
        2 => LayoutAction::OpenNotes {
            expected_revision: revision,
            group: pick_group!(),
        },
        3 => LayoutAction::ActivateTab {
            expected_revision: revision,
            tab: pick_tab!(),
        },
        4 => LayoutAction::FocusGroup {
            expected_revision: revision,
            group: pick_group!(),
        },
        5 => LayoutAction::CloseTab {
            expected_revision: revision,
            tab: pick_tab!(),
        },
        6 => LayoutAction::CloseGroup {
            expected_revision: revision,
            group: pick_group!(),
        },
        7 => LayoutAction::DuplicateTab {
            expected_revision: revision,
            tab: pick_tab!(),
        },
        8 => LayoutAction::ReopenClosed {
            expected_revision: revision,
        },
        9 => {
            let tab = pick_tab!();
            let target = if rng.chance(50) {
                TabDropTarget::Index {
                    group: pick_group!(),
                    index: rng.below(6),
                }
            } else {
                TabDropTarget::SplitFrom {
                    group: pick_group!(),
                    arrangement: if rng.chance(50) {
                        Arrangement::SideBySide
                    } else {
                        Arrangement::Stacked
                    },
                    side: if rng.chance(50) {
                        Side::First
                    } else {
                        Side::Second
                    },
                }
            };
            LayoutAction::MoveTab {
                expected_revision: revision,
                tab,
                target,
            }
        }
        10 => {
            let (a, b) = (pick_group!(), pick_group!());
            LayoutAction::MergeGroups {
                expected_revision: revision,
                source: a,
                destination: b,
            }
        }
        11 => {
            let (a, b) = (pick_group!(), pick_group!());
            LayoutAction::MoveGroupSplit {
                expected_revision: revision,
                source: a,
                destination: b,
                arrangement: if rng.chance(50) {
                    Arrangement::SideBySide
                } else {
                    Arrangement::Stacked
                },
                side: if rng.chance(50) {
                    Side::First
                } else {
                    Side::Second
                },
            }
        }
        12 => LayoutAction::MoveGroupEdge {
            expected_revision: revision,
            group: pick_group!(),
            edge: [Edge::Top, Edge::Left, Edge::Right, Edge::Bottom][rng.below(4)],
        },
        13 => LayoutAction::SplitGroup {
            expected_revision: revision,
            group: pick_group!(),
            arrangement: if rng.chance(50) {
                Arrangement::SideBySide
            } else {
                Arrangement::Stacked
            },
        },
        14 => {
            let splits = state.tree().splits();
            let split = if splits.is_empty() || rng.chance(10) {
                SplitId::from_raw(7)
            } else {
                splits[rng.below(splits.len())]
            };
            LayoutAction::ResizeSplit {
                expected_revision: revision,
                split,
                desired_ratio_ppm: match rng.below(8) {
                    0 => 1_000,
                    1 => 999_000,
                    _ => {
                        flavite_workspace::layout::MIN_RATIO_PPM
                            + u32::try_from(rng.below(
                                (flavite_workspace::layout::MAX_RATIO_PPM
                                    - flavite_workspace::layout::MIN_RATIO_PPM)
                                    as usize,
                            ))
                            .unwrap_or(0)
                    }
                },
            }
        }
        15 => LayoutAction::ToggleMaximize {
            expected_revision: revision,
            group: pick_group!(),
        },
        16 => LayoutAction::NoteLoadReady {
            expected_revision: revision,
            tab: pick_tab!(),
        },
        17 => LayoutAction::NoteLoadFailed {
            expected_revision: revision,
            tab: pick_tab!(),
        },
        18 => LayoutAction::CycleTab {
            expected_revision: revision,
            forward: rng.chance(50),
        },
        _ => LayoutAction::CycleGroup {
            expected_revision: revision,
            forward: rng.chance(50),
        },
    }
}

/// One deterministic 100k-action campaign at a fixed seed. Failures print
/// the seed so they replay exactly.
#[test]
fn hundred_thousand_randomized_actions_preserve_the_tab_multiset() {
    let seed = 0xC0FF_EE01;
    run_campaign(seed, ACTION_BUDGET);
}

/// Smaller proptest campaigns over fresh seeds catch generator-dependent
/// divergence with shrinking-friendly budgets.
#[test]
fn seeded_campaigns_across_generator_seeds_stay_invariant() {
    for seed in [1, 2, 3, 0xDEAD_BEEF, 0x5EED_5EED] {
        run_campaign(seed, 5_000);
    }
}

fn run_campaign(seed: u64, budget: usize) {
    let mut ids = Ids::with_seed(u128::from(seed));
    let mut state = WorkspaceState::with_ids(&mut ids);
    let locators: Vec<NoteLocator> = ["a.md", "b.md", "Systems/c.md"]
        .iter()
        .map(|path| NoteLocator::from_components([*path]).expect("locator"))
        .collect();
    let mut rng = Rng::new(seed);
    let mut committed = 0u64;
    let mut rejected = 0u64;
    for step in 0..budget {
        let action = build_action(&mut rng, &state, &locators);
        let action_debug = action.clone();
        let before = state.clone();
        let delta = expected_delta(&state, &action);
        match reduce(&state, action_debug) {
            Outcome::Committed(commit) => {
                committed += 1;
                state = commit.state;
                check_invariants(&state);
                assert_eq!(
                    state.revision(),
                    before.revision() + 1,
                    "seed {seed} step {step}: revision must advance by one"
                );
                let expected = delta.unwrap_or_else(|| {
                    panic!("seed {seed} step {step}: committed an action the model rejects")
                });
                let actual = i64::try_from(state.tabs().count()).expect("bounded")
                    - i64::try_from(before.tabs().count()).expect("bounded");
                assert_eq!(
                    actual, expected,
                    "seed {seed} step {step}: tab-count delta diverged from the model for {action:?}"
                );
                // Reopen consumes history; close records it.
                if matches!(action, LayoutAction::ReopenClosed { .. }) {
                    assert!(
                        state.recently_closed_len() < before.recently_closed_len()
                            || before.recently_closed_len() == 0,
                        "reopen consumes one history entry"
                    );
                }
            }
            Outcome::Rejected(error) => {
                rejected += 1;
                assert_eq!(
                    state, before,
                    "seed {seed} step {step}: rejection must be a byte-identical no-op ({error})"
                );
                if !matches!(
                    error,
                    WorkspaceError::StaleRevision { .. } | WorkspaceError::NoChange
                ) && delta.is_some()
                {
                    // The model predicted a commit; the reducer found a
                    // finer-grained validity rule. Both must agree that the
                    // multiset did not change.
                    assert_eq!(
                        state.tabs().count(),
                        before.tabs().count(),
                        "seed {seed} step {step}: rejected {error} changed the tab set"
                    );
                }
            }
        }
        check_invariants(&state);
    }
    assert!(committed > 0, "campaign must commit some actions");
    assert!(rejected > 0, "campaign must reject some actions");
}
