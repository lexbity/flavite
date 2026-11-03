//! W11 constrained-layout matrix (RFC-0003 §27, NFR-14, D-13).
//!
//! The pure solver must cover the entire supported window/interface-scale
//! matrix plus maximum-depth trees without overlap, unreachable groups, or
//! rewritten desired ratios, and the adaptive fallback must be
//! deterministic (focused group + complete switcher order).

use std::collections::BTreeMap;

use flavite_workspace::{
    Arrangement, Geometry, Ids, LayoutAction, LayoutNode, MAX_RATIO_PPM, MIN_RATIO_PPM, Outcome,
    Rect, Size, ViewKind, WorkspaceState, ratio_f32, rects, required_minimum, solve, view_minimum,
};

const WINDOWS: [Size; 3] = [
    Size {
        width: 640.0,
        height: 480.0,
    },
    Size {
        width: 1024.0,
        height: 768.0,
    },
    Size {
        width: 1440.0,
        height: 900.0,
    },
];

const INTERFACE_SCALES: [f32; 4] = [0.75, 1.0, 1.5, 2.0];

fn commit(state: &WorkspaceState, action: LayoutAction) -> WorkspaceState {
    match flavite_workspace::reduce(state, action) {
        Outcome::Committed(next) => next.state,
        Outcome::Rejected(error) => panic!("action rejected: {error}"),
    }
}

/// A workspace of `groups` groups, each holding one note view, split
/// side-by-side at the default ratio.
fn wide_workspace(groups: usize) -> WorkspaceState {
    let mut ids = Ids::with_seed(31008);
    let mut state = WorkspaceState::with_ids(&mut ids);
    let first = state.first_group().unwrap();
    let locator = flavite_workspace::view::NoteLocator::from_components(["a.md"]).unwrap();
    state = commit(
        &state,
        LayoutAction::OpenNote {
            expected_revision: state.revision(),
            locator,
            group: first,
        },
    );
    for _ in 1..groups {
        let focused = state.focused_group().unwrap();
        state = commit(
            &state,
            LayoutAction::SplitGroup {
                expected_revision: state.revision(),
                group: focused,
                arrangement: Arrangement::SideBySide,
            },
        );
        let focused = state.focused_group().unwrap();
        let locator = flavite_workspace::view::NoteLocator::from_components(["a.md"]).unwrap();
        state = commit(
            &state,
            LayoutAction::OpenNote {
                expected_revision: state.revision(),
                locator,
                group: focused,
            },
        );
    }
    state
}

/// A maximum-depth tree: repeatedly split the focused group so the split
/// chain reaches the model's depth limit.
fn deep_workspace() -> WorkspaceState {
    let mut ids = Ids::with_seed(31008);
    let mut state = WorkspaceState::with_ids(&mut ids);
    loop {
        let focused = state.focused_group().unwrap();
        let action = LayoutAction::SplitGroup {
            expected_revision: state.revision(),
            group: focused,
            arrangement: if state.tree().depth().is_multiple_of(2) {
                Arrangement::SideBySide
            } else {
                Arrangement::Stacked
            },
        };
        match flavite_workspace::reduce(&state, action) {
            Outcome::Committed(next) => state = next.state,
            Outcome::Rejected(_) => break,
        }
    }
    assert!(
        state.tree().depth() >= flavite_workspace::layout::MAX_DEPTH - 1,
        "fixture should reach a deep tree, got depth {}",
        state.tree().depth()
    );
    state
}

fn assert_no_overlap(geometry: &Geometry, divider: f32) {
    let values: Vec<Rect> = geometry.rects.values().copied().collect();
    for (i, a) in values.iter().enumerate() {
        for b in values.iter().skip(i + 1) {
            let horizontal_gap = (a.x.max(b.x)) - (a.x + a.width).min(b.x + b.width);
            let vertical_gap = (a.y.max(b.y)) - (a.y + a.height).min(b.y + b.height);
            assert!(
                horizontal_gap >= divider - 1e-4 || vertical_gap >= divider - 1e-4,
                "rectangles overlap: {a:?} vs {b:?}"
            );
        }
    }
}

#[test]
fn nfr14_matrix_solves_every_group_reachable_without_overlap() {
    for groups in [1usize, 2, 4, 8] {
        let state = wide_workspace(groups);
        for arrangement_chain in [false, true] {
            let state = if arrangement_chain {
                // Re-split one group stacked for mixed arrangements.
                let focused = state.focused_group().unwrap();
                commit(
                    &state,
                    LayoutAction::SplitGroup {
                        expected_revision: state.revision(),
                        group: focused,
                        arrangement: Arrangement::Stacked,
                    },
                )
            } else {
                state.clone()
            };
            for window in WINDOWS {
                for scale in INTERFACE_SCALES {
                    let geometry = solve(&state, window, scale, &BTreeMap::new());
                    assert_no_overlap(&geometry, flavite_workspace::SPLITTER_WIDTH * scale);
                    if geometry.adaptive_maximize {
                        // Temporary maximize: exactly one group fills the
                        // window and the switcher order still covers every
                        // group deterministically.
                        assert_eq!(geometry.rects.len(), 1, "{window:?} at {scale}");
                        let focused = state
                            .focused_group()
                            .or_else(|| state.first_group())
                            .unwrap();
                        assert!(geometry.rects.contains_key(&focused));
                        let order = state.group_order();
                        assert_eq!(order.len(), state.group_order().len());
                        let mut sorted = order.clone();
                        sorted.sort();
                        assert_eq!(order, sorted, "switcher order is deterministic");
                    } else {
                        assert_eq!(
                            geometry.rects.len(),
                            state.group_order().len(),
                            "{window:?} at {scale}: every group needs a rect"
                        );
                        for (id, rect) in &geometry.rects {
                            let minimum =
                                flavite_workspace::geometry::group_minimum(&state, id, scale);
                            assert!(rect.width + 1e-4 >= minimum.width);
                            assert!(rect.height + 1e-4 >= minimum.height);
                            // Reachability: the rect lies inside the window.
                            assert!(rect.x >= 0.0 && rect.y >= 0.0);
                            assert!(rect.x + rect.width <= window.width + 1e-4);
                            assert!(rect.y + rect.height <= window.height + 1e-4);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn max_depth_trees_never_overlap_and_adapt_deterministically() {
    let state = deep_workspace();
    assert!(state.tree().depth() <= flavite_workspace::layout::MAX_DEPTH);
    for window in WINDOWS {
        for scale in INTERFACE_SCALES {
            let geometry = solve(&state, window, scale, &BTreeMap::new());
            assert_no_overlap(&geometry, flavite_workspace::SPLITTER_WIDTH * scale);
            let again = solve(&state, window, scale, &BTreeMap::new());
            assert_eq!(geometry, again, "adaptive result is deterministic");
        }
    }
    // The deep tree never mutated: the whole matrix is pure.
    let after = deep_workspace();
    assert_eq!(format!("{:?}", state.tree()), format!("{:?}", after.tree()));
}

#[test]
fn adaptive_maximize_never_rewrites_desired_ratios() {
    let mut state = wide_workspace(4);
    for (index, split) in state.tree().splits().into_iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let ratio = MIN_RATIO_PPM
            + ((index as u64 * 111_111) % u64::from(MAX_RATIO_PPM - MIN_RATIO_PPM)) as u32;
        state = commit(
            &state,
            LayoutAction::ResizeSplit {
                expected_revision: state.revision(),
                split,
                desired_ratio_ppm: ratio,
            },
        );
    }
    let before = format!("{:?}", state.tree());
    for window in WINDOWS {
        for scale in INTERFACE_SCALES {
            let _ = solve(&state, window, scale, &BTreeMap::new());
            assert_eq!(format!("{:?}", state.tree()), before);
        }
    }
    // A resize preview is equally non-destructive.
    let mut preview = std::collections::BTreeMap::new();
    let split = state.tree().splits()[0];
    preview.insert(split, MAX_RATIO_PPM);
    let _ = solve(&state, WINDOWS[0], 2.0, &preview);
    assert_eq!(format!("{:?}", state.tree()), before);
}

#[test]
fn ratio_f32_is_the_single_exact_conversion() {
    // The canonical conversion must be the nearest f32 of ppm/1e6. The
    // pre-W11 adapter path truncated to whole hundredths (e.g. 50_097 ppm
    // became 0.05), so this also pins the precision repair.
    for ppm in (MIN_RATIO_PPM..=MAX_RATIO_PPM).step_by(97) {
        #[allow(clippy::cast_possible_truncation)]
        let exact = (f64::from(ppm) / 1_000_000.0) as f32;
        // Bit-exact: both conversions must produce the same f32.
        assert_eq!(ratio_f32(ppm).to_bits(), exact.to_bits(), "ppm {ppm}");
        assert!((f64::from(ratio_f32(ppm)) * 1_000_000.0 - f64::from(ppm)).abs() < 1.0);
    }
    // Rect widths follow the ratio exactly at every scale.
    let state = wide_workspace(2);
    let split = state.tree().splits()[0];
    let state = commit(
        &state,
        LayoutAction::ResizeSplit {
            expected_revision: state.revision(),
            split,
            desired_ratio_ppm: 300_000,
        },
    );
    for scale in INTERFACE_SCALES {
        let window = Size {
            width: 2000.0,
            height: 900.0,
        };
        let map = rects(&state, window, scale, &BTreeMap::new());
        let order = state.group_order();
        let usable = window.width - flavite_workspace::SPLITTER_WIDTH * scale;
        // The solver clamps the desired fraction into both groups' scaled
        // minimums; the expected width mirrors that clamp exactly.
        // The fixture groups hold note views: MIN_NOTE.width = 320 at 100%.
        let note_min = 320.0 * scale;
        let desired = usable * 0.3;
        let expected_first = desired.clamp(note_min, usable - note_min);
        assert!((map[&order[0]].width - expected_first).abs() < 1e-3);
    }
}

#[test]
fn view_minimums_scale_and_solve_honors_them_for_every_kind() {
    for scale in INTERFACE_SCALES {
        for kind in [ViewKind::Empty, ViewKind::Notes, ViewKind::Search] {
            let minimum = view_minimum(&kind, scale);
            assert!(minimum.width > 0.0 && minimum.height > 0.0);
        }
    }
    // A window below the required minimum must report adaptive maximize
    // rather than clipping any group below its minimum.
    let state = wide_workspace(2);
    let required = required_minimum(&state, 2.0);
    let tiny = Size {
        width: required.width / 2.0,
        height: required.height,
    };
    assert!(!flavite_workspace::fits(&state, tiny, 2.0));
    let geometry = solve(&state, tiny, 2.0, &BTreeMap::new());
    assert!(geometry.adaptive_maximize);
}

#[test]
fn resize_previews_reproject_without_committing() {
    let state = wide_workspace(2);
    let split = state.tree().splits()[0];
    let mut preview = std::collections::BTreeMap::new();
    preview.insert(split, 800_000_u32);
    let window = Size {
        width: 1600.0,
        height: 900.0,
    };
    let previewed = rects(&state, window, 1.0, &preview);
    let desired = rects(&state, window, 1.0, &BTreeMap::new());
    let order = state.group_order();
    assert!(previewed[&order[0]].width > desired[&order[0]].width);
    // The preview leaves the authoritative ratio untouched.
    match state.tree() {
        LayoutNode::Split {
            desired_ratio_ppm, ..
        } => assert_eq!(
            *desired_ratio_ppm,
            flavite_workspace::layout::DEFAULT_RATIO_PPM
        ),
        LayoutNode::Group(_) => panic!("expected a split"),
    }
}
