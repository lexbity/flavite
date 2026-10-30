//! Desired/effective geometry for the layout tree (RFC-0003 D-13, §7.5).
//!
//! Persisted desired ratios are never rewritten by adaptive layout: the
//! solver computes minimum constraints bottom-up and effective sizes
//! top-down, and reports when the window cannot satisfy the tree so the
//! adapter can show a temporary maximized group plus a complete group
//! switcher without deleting or relocating tabs.

use crate::ids::{GroupId, SplitId};
use crate::layout::{Arrangement, LayoutNode, MAX_RATIO_PPM, MIN_RATIO_PPM};
use crate::reducer::WorkspaceState;
use crate::view::ViewKind;
use std::collections::BTreeMap;

/// Per-view minimums at 100% interface scale (§7.5): Note 320×200,
/// Notes/Empty 200×160. Tab and title chrome are included in these heights.
const MIN_NOTE: (f32, f32) = (320.0, 200.0);
const MIN_LIST: (f32, f32) = (200.0, 160.0);
const MIN_CONTEXT: (f32, f32) = (240.0, 160.0);

/// A rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

/// A size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

/// Splitter thickness at 100% scale (§7.5). The hit corridor is handled by
/// the adapter; geometry divides by this exact width.
pub const SPLITTER_WIDTH: f32 = 4.0;

/// The minimum size of one view at the given interface scale.
#[must_use]
pub fn view_minimum(kind: &ViewKind, scale: f32) -> Size {
    let (width, height) = match kind {
        ViewKind::Note(_) => MIN_NOTE,
        ViewKind::Notes | ViewKind::Empty => MIN_LIST,
        ViewKind::Preview(_) | ViewKind::Backlinks(_) => MIN_CONTEXT,
    };
    Size {
        width: width * scale,
        height: height * scale,
    }
}

/// The smallest group size that satisfies every member view.
fn group_minimum(state: &WorkspaceState, id: &GroupId, scale: f32) -> Size {
    let Some(group) = state.group(id) else {
        return Size::ZERO;
    };
    let mut minimum = Size::ZERO;
    for tab in group.tabs() {
        if let Some(session) = state.tab(tab) {
            let size = view_minimum(session.kind(), scale);
            minimum = Size {
                width: minimum.width.max(size.width),
                height: minimum.height.max(size.height),
            };
        }
    }
    minimum
}

impl Default for Size {
    fn default() -> Self {
        Self::ZERO
    }
}

impl Size {
    /// Zero size.
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };
}

/// Minimum subtree size, bottom-up (§7.5 equations).
#[must_use]
pub fn subtree_minimum(state: &WorkspaceState, node: &LayoutNode, scale: f32) -> Size {
    match node {
        LayoutNode::Group(id) => group_minimum(state, id, scale),
        LayoutNode::Split {
            arrangement,
            first,
            second,
            ..
        } => {
            let first_min = subtree_minimum(state, first, scale);
            let second_min = subtree_minimum(state, second, scale);
            let divider = SPLITTER_WIDTH * scale;
            match arrangement {
                Arrangement::SideBySide => Size {
                    width: first_min.width + divider + second_min.width,
                    height: first_min.height.max(second_min.height),
                },
                Arrangement::Stacked => Size {
                    width: first_min.width.max(second_min.width),
                    height: first_min.height + divider + second_min.height,
                },
            }
        }
    }
}

/// The smallest window that can display the whole workspace.
#[must_use]
pub fn required_minimum(state: &WorkspaceState, scale: f32) -> Size {
    subtree_minimum(state, state.tree(), scale)
}

/// Whether the workspace fits `available`; when false the adapter must
/// temporarily maximize one group and offer a complete group switcher
/// (D-13) without touching saved ratios.
#[must_use]
pub fn fits(state: &WorkspaceState, available: Size, scale: f32) -> bool {
    let required = required_minimum(state, scale);
    available.width >= required.width && available.height >= required.height
}

/// Effective rectangles for every group, top-down with clamping.
///
/// `preview` carries ephemeral resize ratios (released resizes commit a
/// `ResizeSplit` transaction instead); `scale` multiplies chrome minimums.
/// When the tree cannot fit, the focused group receives the whole region
/// and `adaptive_maximize` is true.
#[must_use]
pub fn solve(
    state: &WorkspaceState,
    available: Size,
    scale: f32,
    preview: &BTreeMap<SplitId, u32>,
) -> Geometry {
    let tree = state.tree();
    let required = required_minimum(state, scale);
    if available.width < required.width || available.height < required.height {
        let focused = state.focused_group().or_else(|| state.first_group());
        let mut rects = BTreeMap::new();
        if let Some(group) = focused {
            rects.insert(
                group,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    width: available.width,
                    height: available.height,
                },
            );
        }
        return Geometry {
            rects,
            adaptive_maximize: true,
        };
    }
    let mut rects = BTreeMap::new();
    solve_node(
        state,
        tree,
        scale,
        preview,
        Rect {
            x: 0.0,
            y: 0.0,
            width: available.width,
            height: available.height,
        },
        &mut rects,
    );
    Geometry {
        rects,
        adaptive_maximize: false,
    }
}

/// Geometry output for one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    /// One rectangle per group.
    pub rects: BTreeMap<GroupId, Rect>,
    /// Whether the workspace only fits by temporarily maximizing a group.
    pub adaptive_maximize: bool,
}

fn solve_node(
    state: &WorkspaceState,
    node: &LayoutNode,
    scale: f32,
    preview: &BTreeMap<SplitId, u32>,
    rect: Rect,
    out: &mut BTreeMap<GroupId, Rect>,
) {
    match node {
        LayoutNode::Group(id) => {
            out.insert(*id, rect);
        }
        LayoutNode::Split {
            id,
            arrangement,
            desired_ratio_ppm,
            first,
            second,
        } => {
            let ratio = preview
                .get(id)
                .copied()
                .unwrap_or(*desired_ratio_ppm)
                .clamp(MIN_RATIO_PPM, MAX_RATIO_PPM);
            let divider = SPLITTER_WIDTH * scale;
            let first_min = subtree_minimum(state, first, scale);
            let second_min = subtree_minimum(state, second, scale);
            match arrangement {
                Arrangement::SideBySide => {
                    let usable = (rect.width - divider).max(0.0);
                    let desired = usable * f32_ratio(ratio) / 1_000_000.0;
                    let first_width =
                        desired.clamp(first_min.width, (usable - second_min.width).max(0.0));
                    solve_node(
                        state,
                        first,
                        scale,
                        preview,
                        Rect {
                            x: rect.x,
                            y: rect.y,
                            width: first_width,
                            height: rect.height,
                        },
                        out,
                    );
                    solve_node(
                        state,
                        second,
                        scale,
                        preview,
                        Rect {
                            x: rect.x + first_width + divider,
                            y: rect.y,
                            width: (usable - first_width).max(0.0),
                            height: rect.height,
                        },
                        out,
                    );
                }
                Arrangement::Stacked => {
                    let usable = (rect.height - divider).max(0.0);
                    let desired = usable * f32_ratio(ratio) / 1_000_000.0;
                    let first_height =
                        desired.clamp(first_min.height, (usable - second_min.height).max(0.0));
                    solve_node(
                        state,
                        first,
                        scale,
                        preview,
                        Rect {
                            x: rect.x,
                            y: rect.y,
                            width: rect.width,
                            height: first_height,
                        },
                        out,
                    );
                    solve_node(
                        state,
                        second,
                        scale,
                        preview,
                        Rect {
                            x: rect.x,
                            y: rect.y + first_height + divider,
                            width: rect.width,
                            height: (usable - first_height).max(0.0),
                        },
                        out,
                    );
                }
            }
        }
    }
}

/// Ratio ppm to f32. The bounded range (`<= 950_000`) is exact in an f32
/// mantissa, so the cast cannot lose precision for stored ratios.
fn f32_ratio(ratio: u32) -> f32 {
    debug_assert!(ratio <= MAX_RATIO_PPM);
    #[allow(clippy::cast_precision_loss)]
    {
        ratio as f32
    }
}

/// Convenience: rectangles only, discarding diagnostics.
#[must_use]
pub fn rects(
    state: &WorkspaceState,
    available: Size,
    scale: f32,
    preview: &BTreeMap<SplitId, u32>,
) -> BTreeMap<GroupId, Rect> {
    solve(state, available, scale, preview).rects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Ids;
    use crate::layout::{Arrangement, DEFAULT_RATIO_PPM, MIN_RATIO_PPM};
    use crate::reducer::{LayoutAction, WorkspaceState};
    use crate::view::NoteLocator;

    fn two_group_workspace(ids: &mut Ids, arrangement: Arrangement) -> WorkspaceState {
        let mut state = WorkspaceState::with_ids(ids);
        let group = state.first_group().unwrap();
        let locator = NoteLocator::from_components(["a.md"]).unwrap();
        for _ in 0..2 {
            match crate::reduce(
                &state,
                LayoutAction::OpenNote {
                    expected_revision: state.revision(),
                    locator: locator.clone(),
                    group,
                },
            ) {
                crate::Outcome::Committed(commit) => state = commit.state,
                crate::Outcome::Rejected(error) => panic!("{error}"),
            }
        }
        match crate::reduce(
            &state,
            LayoutAction::SplitGroup {
                expected_revision: state.revision(),
                group,
                arrangement,
            },
        ) {
            crate::Outcome::Committed(commit) => commit.state,
            crate::Outcome::Rejected(error) => panic!("{error}"),
        }
    }

    #[test]
    fn side_by_side_divides_horizontally_with_a_vertical_divider() {
        let mut ids = Ids::with_seed(4);
        let state = two_group_workspace(&mut ids, Arrangement::SideBySide);
        let available = Size {
            width: 1000.0,
            height: 500.0,
        };
        let rects = rects(&state, available, 1.0, &BTreeMap::new());
        assert_eq!(rects.len(), 2);
        let order = state.group_order();
        let (left, right) = (rects[&order[0]], rects[&order[1]]);
        assert!(left.x.abs() < 1e-4);
        assert!(left.y.abs() < 1e-4);
        assert!((left.height - 500.0).abs() < 1e-4);
        assert!((right.height - 500.0).abs() < 1e-4);
        // Divider width exactly separates the rectangles.
        assert!((right.x - (left.x + left.width) - SPLITTER_WIDTH).abs() < 1e-4);
        assert!((left.width + right.width + SPLITTER_WIDTH - 1000.0).abs() < 1e-4);
        // 50% desired ratio at equal minimums splits usable width evenly.
        assert!((left.width - right.width).abs() < 1e-4);
    }

    #[test]
    fn stacked_divides_vertically_with_a_horizontal_divider() {
        let mut ids = Ids::with_seed(4);
        let state = two_group_workspace(&mut ids, Arrangement::Stacked);
        let available = Size {
            width: 800.0,
            height: 600.0,
        };
        let rects = rects(&state, available, 1.0, &BTreeMap::new());
        let order = state.group_order();
        let (top, bottom) = (rects[&order[0]], rects[&order[1]]);
        assert!((top.width - 800.0).abs() < 1e-4);
        assert!((bottom.width - 800.0).abs() < 1e-4);
        assert!(top.y.abs() < 1e-4);
        assert!((bottom.y - (top.y + top.height) - SPLITTER_WIDTH).abs() < 1e-4);
        assert!((top.height + bottom.height + SPLITTER_WIDTH - 600.0).abs() < 1e-4);
    }

    fn assert_no_overlap(rects: &BTreeMap<GroupId, Rect>, divider: f32) {
        let values: Vec<Rect> = rects.values().copied().collect();
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
    fn rectangles_never_overlap_across_scales_and_ratios() {
        for scale in [0.75, 1.0, 1.5, 2.0] {
            for ratio in [
                MIN_RATIO_PPM,
                DEFAULT_RATIO_PPM,
                crate::layout::MAX_RATIO_PPM,
            ] {
                for arrangement in [Arrangement::SideBySide, Arrangement::Stacked] {
                    let mut ids = Ids::with_seed(9);
                    let mut state = two_group_workspace(&mut ids, arrangement);
                    let split = state.tree().splits()[0];
                    state = match crate::reduce(
                        &state,
                        LayoutAction::ResizeSplit {
                            expected_revision: state.revision(),
                            split,
                            desired_ratio_ppm: ratio,
                        },
                    ) {
                        crate::Outcome::Committed(commit) => commit.state,
                        crate::Outcome::Rejected(error) => panic!("{error}"),
                    };
                    let available = Size {
                        width: 2000.0,
                        height: 1200.0,
                    };
                    let rects = rects(&state, available, scale, &BTreeMap::new());
                    assert_eq!(rects.len(), 2, "scale {scale} ratio {ratio}");
                    assert_no_overlap(&rects, SPLITTER_WIDTH * scale);
                    for (id, rect) in &rects {
                        let minimum = group_minimum(&state, id, scale);
                        assert!(
                            rect.width + 1e-4 >= minimum.width,
                            "{rect:?} vs {minimum:?}"
                        );
                        assert!(
                            rect.height + 1e-4 >= minimum.height,
                            "{rect:?} vs {minimum:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn minimums_are_respected_when_clamped() {
        let mut ids = Ids::with_seed(4);
        let state = two_group_workspace(&mut ids, Arrangement::SideBySide);
        // Extreme desired ratio: the clamp must honor the second group's
        // minimum width instead of following the ratio off the edge.
        let split = state.tree().splits()[0];
        let state = match crate::reduce(
            &state,
            LayoutAction::ResizeSplit {
                expected_revision: state.revision(),
                split,
                desired_ratio_ppm: crate::layout::MAX_RATIO_PPM,
            },
        ) {
            crate::Outcome::Committed(commit) => commit.state,
            crate::Outcome::Rejected(error) => panic!("{error}"),
        };
        let available = Size {
            width: 700.0,
            height: 400.0,
        };
        let rects = rects(&state, available, 1.0, &BTreeMap::new());
        let order = state.group_order();
        let (left, right) = (rects[&order[0]], rects[&order[1]]);
        assert!(
            right.width >= MIN_LIST.0 - 1e-4,
            "second group keeps its minimum"
        );
        assert!(left.width + right.width + SPLITTER_WIDTH <= 700.0 + 1e-4);
    }

    #[test]
    fn a_window_too_small_reports_adaptive_maximize_without_mutating_ratios() {
        let mut ids = Ids::with_seed(4);
        let state = two_group_workspace(&mut ids, Arrangement::SideBySide);
        let before = state.clone();
        let available = Size {
            width: 300.0,
            height: 300.0,
        };
        assert!(!fits(&state, available, 1.0));
        let geometry = solve(&state, available, 1.0, &BTreeMap::new());
        assert!(geometry.adaptive_maximize);
        assert_eq!(geometry.rects.len(), 1, "one maximized group is shown");
        // Ratios in the authoritative state are untouched by adaptive layout.
        assert_eq!(state, before);
        match state.tree() {
            LayoutNode::Split {
                desired_ratio_ppm, ..
            } => assert_eq!(*desired_ratio_ppm, DEFAULT_RATIO_PPM),
            LayoutNode::Group(_) => panic!("expected split, got a lone group"),
        }
    }

    #[test]
    fn preview_ratios_override_desired_without_a_commit() {
        let mut ids = Ids::with_seed(4);
        let state = two_group_workspace(&mut ids, Arrangement::SideBySide);
        let split = state.tree().splits()[0];
        let mut preview = BTreeMap::new();
        preview.insert(split, 900_000_u32);
        let available = Size {
            width: 1000.0,
            height: 500.0,
        };
        let previewed = rects(&state, available, 1.0, &preview);
        let desired = rects(&state, available, 1.0, &BTreeMap::new());
        let order = state.group_order();
        assert!(previewed[&order[0]].width > desired[&order[0]].width);
        assert!(
            (previewed[&order[0]].width + previewed[&order[1]].width + SPLITTER_WIDTH
                - available.width)
                .abs()
                < 1e-4
        );
    }
}
