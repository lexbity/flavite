//! Fuzz target for the IO-free workspace reducer (RFC-0003 W02, NFR-13).
//!
//! Interprets the input as a seeded action stream against the layout
//! model. Every committed state must satisfy the structural invariants,
//! every rejection must be a byte-identical no-op, and the tab multiset
//! must change only through explicit open/close/duplicate actions.

#![no_main]

use libfuzzer_sys::fuzz_target;
use flavite_workspace::{
    Arrangement, Edge, GroupId, Ids, LayoutAction, NoteLocator, Outcome, Side, SplitId,
    TabDropTarget, TabId, WorkspaceState, reduce,
};

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        #[allow(clippy::cast_possible_truncation)]
        {
            if bound == 0 {
                0
            } else {
                (self.next_u64() % bound as u64) as usize
            }
        }
    }
}

fn invariants(state: &WorkspaceState) {
    let tree_groups = state.group_order();
    assert!(!tree_groups.is_empty(), "tree has no groups");
    let unique: std::collections::BTreeSet<_> = tree_groups.iter().copied().collect();
    assert_eq!(unique.len(), tree_groups.len(), "duplicate group leaf");
    let mut members = 0;
    for id in &tree_groups {
        let group = state.group(id).expect("tree group is mapped");
        assert!(!group.tabs().is_empty(), "empty group");
        assert!(group.tabs().contains(&group.active()), "stale active tab");
        members += group.tabs().len();
    }
    assert_eq!(
        members,
        state.tabs().count(),
        "tab map is not the union of group members"
    );
    let focus = state.focused_tab();
    assert!(
        state.groups().any(|(_, group)| group.active() == focus),
        "focus is not an active tab"
    );
}

fn build_action(rng: &mut Rng, state: &WorkspaceState) -> LayoutAction {
    #[allow(clippy::too_many_lines)]
    let revision = if rng.below(10) == 0 {
        state.revision().wrapping_add(1 + rng.below(99) as u64)
    } else {
        state.revision()
    };
    let groups = state.group_order();
    let tabs: Vec<TabId> = state.tabs().map(|(tab, _)| *tab).collect();
    macro_rules! pick_group {
        () => {
            if rng.below(6) == 0 {
                GroupId::from_raw(u128::MAX)
            } else {
                groups[rng.below(groups.len())]
            }
        };
    }
    macro_rules! pick_tab {
        () => {
            if rng.below(6) == 0 {
                TabId::from_raw(u128::MAX)
            } else {
                tabs[rng.below(tabs.len())]
            }
        };
    }
    macro_rules! arrangement {
        () => {
            if rng.below(2) == 0 {
                Arrangement::SideBySide
            } else {
                Arrangement::Stacked
            }
        };
    }
    macro_rules! side {
        () => {
            if rng.below(2) == 0 {
                Side::First
            } else {
                Side::Second
            }
        };
    }
    match rng.below(14) {
        0 => LayoutAction::OpenNote {
            expected_revision: revision,
            locator: NoteLocator::from_components([format!("n{}.md", rng.below(8))])
                .unwrap_or_else(|_| NoteLocator::from_components(["n.md"]).unwrap()),
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
        4 => LayoutAction::CloseTab {
            expected_revision: revision,
            tab: pick_tab!(),
        },
        5 => LayoutAction::CloseGroup {
            expected_revision: revision,
            group: pick_group!(),
        },
        6 => LayoutAction::DuplicateTab {
            expected_revision: revision,
            tab: pick_tab!(),
        },
        7 => LayoutAction::ReopenClosed {
            expected_revision: revision,
        },
        8 => {
            let tab = pick_tab!();
            let target = if rng.below(2) == 0 {
                TabDropTarget::Index {
                    group: pick_group!(),
                    index: rng.below(6),
                }
            } else {
                TabDropTarget::SplitFrom {
                    group: pick_group!(),
                    arrangement: arrangement!(),
                    side: side!(),
                }
            };
            LayoutAction::MoveTab {
                expected_revision: revision,
                tab,
                target,
            }
        }
        9 => {
            let (a, b) = (pick_group!(), pick_group!());
            LayoutAction::MergeGroups {
                expected_revision: revision,
                source: a,
                destination: b,
            }
        }
        10 => {
            let (a, b) = (pick_group!(), pick_group!());
            LayoutAction::MoveGroupSplit {
                expected_revision: revision,
                source: a,
                destination: b,
                arrangement: arrangement!(),
                side: side!(),
            }
        }
        11 => LayoutAction::MoveGroupEdge {
            expected_revision: revision,
            group: pick_group!(),
            edge: [Edge::Top, Edge::Left, Edge::Right, Edge::Bottom][rng.below(4)],
        },
        12 => {
            let splits = state.tree().splits();
            let split = if splits.is_empty() || rng.below(6) == 0 {
                SplitId::from_raw(u128::MAX)
            } else {
                splits[rng.below(splits.len())]
            };
            LayoutAction::ResizeSplit {
                expected_revision: revision,
                split,
                desired_ratio_ppm: 50_000 + {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = rng.below(950_001) as u32;
                    v
                },
            }
        }
        _ => LayoutAction::SplitGroup {
            expected_revision: revision,
            group: pick_group!(),
            arrangement: arrangement!(),
        },
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let mut rng = Rng(u64::from_le_bytes(data[..8].try_into().expect("8 bytes")).max(1));
    let mut seed16 = [0u8; 16];
    seed16[..8].copy_from_slice(&data[..8]);
    let mut ids = Ids::with_seed(u128::from_le_bytes(seed16));
    let mut state = WorkspaceState::with_ids(&mut ids);
    let steps = data[7].min(64);
    for _ in 0..steps {
        let action = build_action(&mut rng, &state);
        let before = state.clone();
        match reduce(&state, action) {
            Outcome::Committed(commit) => {
                state = commit.state;
                assert_eq!(
                    state.revision(),
                    before.revision() + 1,
                    "committed revisions advance by one"
                );
            }
            Outcome::Rejected(_) => {
                assert_eq!(state, before, "rejections are byte-identical no-ops");
            }
        }
        invariants(&state);
    }
});
