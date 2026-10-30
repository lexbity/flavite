//! Tab drag gesture recognition (RFC-0003 §9.3 adapted to tab strips).
//!
//! A press on a tab starts a candidate. Movement past the 4-logical-pixel
//! threshold turns it into a drag; the hovered tab (reported by the tab's
//! own move events) becomes the insertion target. Release dispatches one
//! [`LayoutAction::MoveTab`]; Escape cancels without mutation.

use flavite_workspace::{GroupId, TabId};
use iced::{Point, Size};

/// Movement threshold before a press becomes a drag (§9.3: 4 logical px).
pub const DRAG_THRESHOLD: f32 = 4.0;

/// The tab-drag state machine.
#[derive(Debug, Clone, Default)]
pub struct TabGesture {
    state: State,
}

#[derive(Debug, Clone, Default)]
enum State {
    #[default]
    Idle,
    /// Pressed with a candidate tab; the origin anchors the threshold.
    Pressed { tab: TabId, origin: Option<Point> },
    /// Dragging: the last hovered target is the drop candidate.
    Dragging { tab: TabId, target: Option<Target> },
}

/// Where a dragged tab would land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// Destination group.
    pub group: GroupId,
    /// Destination index (before the hovered tab, or append).
    pub index: usize,
    /// An edge drop creates a new split.
    pub edge: Option<flavite_workspace::Edge>,
}

impl TabGesture {
    /// A press on a tab begins a drag candidate. The origin is unknown
    /// until the first move event (the press message carries no position).
    pub fn press(&mut self, tab: TabId) {
        self.state = State::Pressed { tab, origin: None };
    }

    /// Pointer movement. The first movement anchors the origin; movement
    /// past the threshold turns the candidate into a drag.
    pub fn moved(&mut self, position: Point) {
        match &mut self.state {
            State::Pressed { origin, .. } if origin.is_none() => {
                *origin = Some(position);
            }
            State::Pressed { tab, origin } => {
                let Some(start) = origin else { return };
                let distance = (position.x - start.x).hypot(position.y - start.y);
                if distance > DRAG_THRESHOLD {
                    let tab = *tab;
                    self.state = State::Dragging { tab, target: None };
                }
            }
            State::Dragging { .. } => {}
            State::Idle => {}
        }
    }

    /// A hovered tab during a drag becomes the insertion target.
    pub fn hover(&mut self, group: GroupId, index: usize) {
        if let State::Dragging { target, .. } = &mut self.state {
            *target = Some(Target {
                group,
                index,
                edge: None,
            });
        }
    }

    /// Hovering a group body appends to that group.
    pub fn hover_group(&mut self, group: GroupId, group_tab_count: usize) {
        if let State::Dragging { target, .. } = &mut self.state {
            *target = Some(Target {
                group,
                index: group_tab_count,
                edge: None,
            });
        }
    }

    /// Replace the candidate after a hit test using actual mounted rectangles.
    pub fn set_target(&mut self, next: Option<Target>) {
        if let State::Dragging { target, .. } = &mut self.state {
            *target = next;
        }
    }

    /// Whether a drag is active (drives drop-zone highlighting and the
    /// raw-pointer subscription).
    #[must_use]
    pub const fn dragging(&self) -> bool {
        matches!(self.state, State::Dragging { .. })
    }

    /// The dragged tab, if a drag is active.
    #[must_use]
    pub const fn dragged_tab(&self) -> Option<TabId> {
        match self.state {
            State::Dragging { tab, .. } => Some(tab),
            _ => None,
        }
    }

    /// The current drop candidate.
    #[must_use]
    pub const fn target(&self) -> Option<Target> {
        match self.state {
            State::Dragging { target, .. } => target,
            _ => None,
        }
    }

    /// Release the pointer: the resolved move, if any.
    pub fn release(&mut self) -> Option<(TabId, Target)> {
        match std::mem::take(&mut self.state) {
            State::Dragging { tab, target } => target.map(|target| (tab, target)),
            _ => None,
        }
    }

    /// Escape or losing pointer ownership cancels without mutation.
    pub fn cancel(&mut self) {
        self.state = State::Idle;
    }

    /// Window size changes never alter the gesture; recorded for boundary
    /// assertions in tests.
    #[must_use]
    pub const fn threshold_for(_window: Size) -> f32 {
        DRAG_THRESHOLD
    }
}

impl Target {
    #[must_use]
    pub fn layout_target(self) -> flavite_workspace::TabDropTarget {
        match self.edge {
            Some(edge) => flavite_workspace::TabDropTarget::SplitFrom {
                group: self.group,
                arrangement: edge.arrangement(),
                side: edge.side(),
            },
            None => flavite_workspace::TabDropTarget::Index {
                group: self.group,
                index: self.index,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    #[test]
    fn small_movements_stay_presses() {
        let mut gesture = TabGesture::default();
        gesture.press(TabId::from_raw(1));
        gesture.moved(point(10.0, 10.0));
        gesture.moved(point(12.0, 11.0));
        assert!(!gesture.dragging());
        assert!(gesture.release().is_none());
    }

    #[test]
    fn threshold_crossing_starts_a_drag_and_hover_sets_the_target() {
        let mut gesture = TabGesture::default();
        gesture.press(TabId::from_raw(1));
        gesture.moved(point(10.0, 10.0));
        gesture.moved(point(20.0, 10.0));
        assert!(gesture.dragging());
        assert_eq!(gesture.dragged_tab(), Some(TabId::from_raw(1)));
        assert_eq!(gesture.target(), None);
        gesture.hover(GroupId::from_raw(2), 1);
        assert_eq!(
            gesture.target(),
            Some(Target {
                group: GroupId::from_raw(2),
                index: 1,
                edge: None,
            })
        );
    }

    #[test]
    fn release_resolves_one_move_and_cancellation_leaves_nothing() {
        let mut gesture = TabGesture::default();
        gesture.press(TabId::from_raw(1));
        gesture.moved(point(10.0, 0.0));
        gesture.moved(point(30.0, 0.0));
        gesture.hover(GroupId::from_raw(2), 0);
        let resolved = gesture.release();
        assert_eq!(
            resolved,
            Some((
                TabId::from_raw(1),
                Target {
                    group: GroupId::from_raw(2),
                    index: 0,
                    edge: None,
                }
            ))
        );
        assert!(!gesture.dragging());

        let mut cancelled = TabGesture::default();
        cancelled.press(TabId::from_raw(1));
        cancelled.moved(point(10.0, 0.0));
        cancelled.moved(point(30.0, 0.0));
        cancelled.cancel();
        assert!(cancelled.release().is_none());
    }
}
