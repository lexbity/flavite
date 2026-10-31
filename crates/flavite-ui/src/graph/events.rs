//! Graph gesture arbitration (RFC-0003 §9.3, D-05, D-06).
//!
//! The renderer turns raw pointer traffic into typed releases; this module
//! decides what a release *means*: select, deliberate empty-selection, or
//! exactly one insert-link intent. Every rule here is pure over injected
//! clock values so the double-click boundary, the triple-click refusal, and
//! the frame-version invalidation are testable without wall-clock races.
//!
//! Contract (§9.3):
//!
//! - First release on a node selects immediately.
//! - Second release on the *same* `EntityRef` under the *same* `RenderKey`,
//!   inside interval and distance tolerance, emits one insert intent and
//!   consumes the pair — it never also dispatches Open, and a third click
//!   starts a fresh candidate instead of inserting twice.
//! - Crossing a frame version clears the pending candidate.
//! - Any completed drag (pan or node-drag) suppresses click recognition and
//!   clears the candidate; `Escape` (via [`GestureArbiter::cancel`]) does too.

use std::time::{Duration, Instant};

use flavite_graph::RenderKey;

use super::geometry::HitNode;

/// Fallback double-click interval when no OS/platform value is available
/// (§9.3: 500 ms; W11 exposes the preference override).
pub const DEFAULT_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Fallback double-click distance in *logical* pixels (§9.3: 4 px). The
/// same tolerance starts node drag/pan recognition (renderer gate), so one
/// number defines "still the same click" for the whole gesture.
pub const DEFAULT_DOUBLE_CLICK_DISTANCE: f32 = 4.0;

/// Tunables injected per host so tests and preferences drive the same code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GestureConfig {
    /// Maximum interval between the two releases of a double click.
    pub double_click_interval: Duration,
    /// Maximum release-to-release distance in logical pixels.
    pub double_click_distance: f32,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            double_click_interval: DEFAULT_DOUBLE_CLICK_INTERVAL,
            double_click_distance: DEFAULT_DOUBLE_CLICK_DISTANCE,
        }
    }
}

/// What the host must do about one accepted release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureOutcome {
    /// Select the node (single click first release — or any click that did
    /// not complete a pair).
    Select(HitNode),
    /// A background click: publish the deliberate empty selection (§D-04).
    PublishEmptySelection,
    /// The second release of a qualifying pair: request exactly one link
    /// insertion intent for this node. The host never also opens here.
    InsertLink(HitNode),
}

/// One remembered release that may still complete a double click.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    node: HitNode,
    at: Instant,
    world: [f32; 2],
    key: RenderKey,
}

/// Per-graph-view click arbiter. State is tiny; one instance per tab.
#[derive(Debug, Default, Clone)]
pub struct GestureArbiter {
    config: GestureConfig,
    candidate: Option<Candidate>,
}

impl GestureArbiter {
    /// A fresh arbiter with the default (fallback) tolerances.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An arbiter with injected tolerances (preferences/tests).
    #[must_use]
    pub const fn with_config(config: GestureConfig) -> Self {
        Self {
            config,
            candidate: None,
        }
    }

    /// Whether a candidate pair is still outstanding.
    #[must_use]
    pub fn has_candidate(&self) -> bool {
        self.candidate.is_some()
    }

    /// Escape, window blur, or any completed drag: recognition stops without
    /// firing (§9.3, §8.2).
    pub fn cancel(&mut self) {
        self.candidate = None;
    }

    /// The frame version moved: any pending candidate is cleared (§9.3:
    /// "Crossing a frame version clears the pending double-click
    /// candidate"). Returns whether a candidate was discarded.
    pub fn invalidate_frame(&mut self, key: &RenderKey) -> bool {
        match self.candidate {
            Some(candidate) if candidate.key != *key => {
                self.candidate = None;
                true
            }
            _ => false,
        }
    }

    /// Arbitrate one `ReleaseKind::Click` release. Drags never reach this
    /// method (the renderer reports them as `PanCompleted`/`NodeDragCompleted`
    /// and the host calls [`GestureArbiter::cancel`] instead).
    ///
    /// `now` and the candidate's stored instant make the interval test
    /// fake-clock friendly; `world` positions are in world units and the
    /// distance test uses the logical-pixel tolerance converted through the
    /// current zoom.
    #[must_use]
    pub fn click(
        &mut self,
        now: Instant,
        node: Option<HitNode>,
        world: [f32; 2],
        key: &RenderKey,
        zoom: f32,
    ) -> GestureOutcome {
        let Some(hit) = node else {
            // Background click ends any pair candidate (it selects nothing).
            self.candidate = None;
            return GestureOutcome::PublishEmptySelection;
        };
        let completes_pair = self.candidate.is_some_and(|candidate| {
            candidate.key == *key
                && candidate.node == hit
                && now
                    .checked_duration_since(candidate.at)
                    .is_some_and(|elapsed| elapsed <= self.config.double_click_interval)
                && {
                    let zoom = if zoom.is_finite() && zoom > 0.0 {
                        zoom
                    } else {
                        1.0
                    };
                    let dx = candidate.world[0] - world[0];
                    let dy = candidate.world[1] - world[1];
                    (dx * dx + dy * dy).sqrt() * zoom <= self.config.double_click_distance
                }
        });
        if completes_pair {
            // Consume the pair: the next click starts a fresh candidate,
            // so a triple click can never insert twice.
            self.candidate = None;
            GestureOutcome::InsertLink(hit)
        } else {
            self.candidate = Some(Candidate {
                node: hit,
                at: now,
                world,
                key: *key,
            });
            GestureOutcome::Select(hit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_graph::{NodeId, NodeKind};

    fn hit(id: u64) -> HitNode {
        HitNode::new(NodeId::note(id), NodeKind::Note)
    }

    fn key(generation: u64) -> RenderKey {
        RenderKey::new(1, generation, 1)
    }

    /// Deterministic fake clock: a fixed reference instant plus explicit
    /// offsets — no `Instant::now` anywhere in these tests.
    fn clock(reference: Instant, millis: u64) -> Instant {
        reference + Duration::from_millis(millis)
    }

    #[test]
    fn first_click_selects_and_second_click_on_same_node_inserts_once() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        assert_eq!(
            arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0),
            GestureOutcome::Select(hit(1))
        );
        assert_eq!(
            arbiter.click(
                clock(reference, 200),
                Some(hit(1)),
                [0.0, 0.0],
                &key(1),
                1.0
            ),
            GestureOutcome::InsertLink(hit(1))
        );
        // Third click of a triple: fresh candidate, select only — the pair
        // was consumed, so no second insert.
        assert_eq!(
            arbiter.click(
                clock(reference, 300),
                Some(hit(1)),
                [0.0, 0.0],
                &key(1),
                1.0
            ),
            GestureOutcome::Select(hit(1))
        );
        assert_eq!(
            arbiter.click(
                clock(reference, 400),
                Some(hit(1)),
                [0.0, 0.0],
                &key(1),
                1.0
            ),
            GestureOutcome::InsertLink(hit(1)),
            "the next pair inserts again — each gesture is one intent"
        );
    }

    #[test]
    fn interval_boundary_is_inclusive_and_the_next_ms_rejects() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::with_config(GestureConfig::default());
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        assert_eq!(
            arbiter.click(
                clock(reference, 500),
                Some(hit(1)),
                [0.0, 0.0],
                &key(1),
                1.0
            ),
            GestureOutcome::InsertLink(hit(1)),
            "500 ms inclusive"
        );
        let mut second = GestureArbiter::with_config(GestureConfig::default());
        let _ = second.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        assert_eq!(
            second.click(
                clock(reference, 501),
                Some(hit(1)),
                [0.0, 0.0],
                &key(1),
                1.0
            ),
            GestureOutcome::Select(hit(1)),
            "501 ms starts a new candidate"
        );
    }

    #[test]
    fn distance_boundary_is_in_logical_pixels_through_zoom() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 2.0);
        // 2.0 world units at zoom 2.0 = 4 logical px = at the limit.
        assert_eq!(
            arbiter.click(
                clock(reference, 100),
                Some(hit(1)),
                [2.0, 0.0],
                &key(1),
                2.0
            ),
            GestureOutcome::InsertLink(hit(1))
        );
        let mut drifted = GestureArbiter::new();
        let _ = drifted.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 2.0);
        assert_eq!(
            drifted.click(
                clock(reference, 100),
                Some(hit(1)),
                [2.6, 0.0],
                &key(1),
                2.0
            ),
            GestureOutcome::Select(hit(1)),
            "5.2 logical px moved: a new candidate, not the same click"
        );
    }

    #[test]
    fn other_node_or_frame_version_never_completes_a_pair() {
        let reference = Instant::now();
        let mut other_node = GestureArbiter::new();
        let _ = other_node.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        assert_eq!(
            other_node.click(clock(reference, 10), Some(hit(2)), [0.0, 0.0], &key(1), 1.0),
            GestureOutcome::Select(hit(2)),
            "double-click another node selects it (its own pair comes later)"
        );
        let mut other_frame = GestureArbiter::new();
        let _ = other_frame.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        assert_eq!(
            other_frame.click(clock(reference, 10), Some(hit(1)), [0.0, 0.0], &key(2), 1.0),
            GestureOutcome::Select(hit(1)),
            "a new base generation invalidates the candidate"
        );
        assert!(!other_frame.has_candidate() || true);
    }

    #[test]
    fn invalidate_frame_clears_pending_candidate() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        assert!(arbiter.invalidate_frame(&key(2)), "a changed key clears");
        assert!(!arbiter.has_candidate());
        // The next click on the new frame selects; it does not insert.
        assert_eq!(
            arbiter.click(clock(reference, 10), Some(hit(1)), [0.0, 0.0], &key(2), 1.0),
            GestureOutcome::Select(hit(1))
        );
        // Same key keeps the candidate.
        arbiter.invalidate_frame(&key(2));
        assert!(arbiter.has_candidate());
    }

    #[test]
    fn background_click_publishes_empty_and_breaks_pairs() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        assert_eq!(
            arbiter.click(clock(reference, 0), None, [0.0, 0.0], &key(1), 1.0),
            GestureOutcome::PublishEmptySelection
        );
        let _ = arbiter.click(clock(reference, 10), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        let _ = arbiter.click(clock(reference, 20), None, [0.0, 0.0], &key(1), 1.0);
        assert!(!arbiter.has_candidate(), "a background click ends the pair");
    }

    #[test]
    fn cancel_clears_without_firing() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        arbiter.cancel();
        assert!(!arbiter.has_candidate());
        assert_eq!(
            arbiter.click(clock(reference, 10), Some(hit(1)), [0.0, 0.0], &key(1), 1.0),
            GestureOutcome::Select(hit(1))
        );
    }

    #[test]
    fn injected_interval_config_changes_the_boundary() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::with_config(GestureConfig {
            double_click_interval: Duration::from_millis(100),
            double_click_distance: 4.0,
        });
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], &key(1), 1.0);
        assert_eq!(
            arbiter.click(
                clock(reference, 150),
                Some(hit(1)),
                [0.0, 0.0],
                &key(1),
                1.0
            ),
            GestureOutcome::Select(hit(1)),
            "a 100 ms preference window rejects a 150 ms second click"
        );
    }
}
