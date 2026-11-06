//! Graph gesture arbitration (RFC-0006 D-02, amending RFC-0003 §9.3).
//!
//! The renderer turns raw pointer traffic into typed releases; this module
//! decides what a release *means*: select, deliberate empty-selection, or a
//! completed activation pair. Since RFC-0006 S04 the double-click pair
//! **activates** (opens) the node — it never mutates text. Insertion is a
//! distinct explicit intent (`GraphCommand::Insert*`), never a gesture
//! outcome. Every rule here is pure over injected clock values so the
//! double-click boundary, the triple-click refusal, and the epoch
//! invalidation are testable without wall-clock races.
//!
//! Contract (D-02):
//!
//! - First release on a node selects immediately.
//! - Second release on the *same* entity under the *same* identity epoch,
//!   inside interval and distance tolerance, emits one activation intent
//!   and consumes the pair — a third click starts a fresh candidate
//!   instead of activating twice.
//! - The navigation pair is keyed by **view incarnation, identity epoch and
//!   entity — not every overlay/generation revision** (D-02): layout
//!   movement inside the bounded hold does not cancel a pair. A newer
//!   identity epoch cancels the pair rather than acting against obsolete
//!   entities.
//! - The candidate hold is bounded by the same 500 ms interval that
//!   completes it: past the window the geometry is no longer frozen and a
//!   click starts a new candidate.
//! - Any completed drag (pan or node-drag) suppresses click recognition and
//!   clears the candidate; `Escape` (via [`GestureArbiter::cancel`]) does too.

use std::time::{Duration, Instant};

use super::geometry::HitNode;

/// Fallback double-click interval when no OS/platform value is available
/// (§9.3: 500 ms; W11 exposes the preference override). This is also the
/// bounded hit-geometry freeze window for one click pair (D-02).
pub const DEFAULT_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Fallback double-click distance in *logical* pixels (§9.3: 4 px). The
/// same tolerance starts node drag/pan recognition (renderer gate), so one
/// number defines "still the same click" for the whole gesture.
pub const DEFAULT_DOUBLE_CLICK_DISTANCE: f32 = 4.0;

/// Tunables injected per host so tests and preferences drive the same code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GestureConfig {
    /// Maximum interval between the two releases of a double click, and the
    /// bounded hold of the frozen hit geometry (D-02).
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
    /// The second release of a qualifying pair: **activate** the node —
    /// navigation through the typed open-intent policy. The host never
    /// mutates a document here and never also inserts (D-02).
    ActivateNode(HitNode),
}

/// One remembered release that may still complete a double click. The pair
/// key is the identity epoch plus the entity — frame/layout generations
/// inside the hold do not matter (D-02 navigation pairing).
#[derive(Debug, Clone, Copy)]
struct Candidate {
    node: HitNode,
    at: Instant,
    world: [f32; 2],
    epoch: u64,
}

/// Per-graph-view click arbiter. State is tiny; one instance per tab. The
/// owning view's incarnation is validated by the message router before the
/// arbiter is reached, so a replaced view cannot complete a pair.
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

    /// Whether a candidate pair is still outstanding (the hit geometry is
    /// frozen for this tab).
    #[must_use]
    pub fn has_candidate(&self) -> bool {
        self.candidate.is_some()
    }

    /// The identity epoch a frozen candidate is still valid for.
    #[must_use]
    pub fn candidate_epoch(&self) -> Option<u64> {
        self.candidate.map(|candidate| candidate.epoch)
    }

    /// Escape, window blur, or any completed drag: recognition stops without
    /// firing (§9.3, §8.2).
    pub fn cancel(&mut self) {
        self.candidate = None;
    }

    /// D-02: the hit-geometry hold is bounded by the same interval that
    /// completes the pair. A candidate whose window elapsed (the second
    /// release never arrived) is dropped, releasing any presentation the
    /// host froze for it. Returns whether a candidate was discarded.
    pub fn expire(&mut self, now: Instant) -> bool {
        match self.candidate {
            Some(candidate)
                if now
                    .checked_duration_since(candidate.at)
                    .is_some_and(|elapsed| elapsed > self.config.double_click_interval) =>
            {
                self.candidate = None;
                true
            }
            _ => false,
        }
    }

    /// The identity epoch moved: any pending candidate is cancelled rather
    /// than acting against obsolete entities (D-02). Mere layout/overlay
    /// generation movement does **not** cancel a navigation pair. Returns
    /// whether a candidate was discarded.
    pub fn invalidate_epoch(&mut self, epoch: u64) -> bool {
        match self.candidate {
            Some(candidate) if candidate.epoch != epoch => {
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
    /// current zoom. `epoch` is the current base identity epoch: a pair
    /// never completes across an epoch boundary.
    #[must_use]
    pub fn click(
        &mut self,
        now: Instant,
        node: Option<HitNode>,
        world: [f32; 2],
        epoch: u64,
        zoom: f32,
    ) -> GestureOutcome {
        let Some(hit) = node else {
            // Background click ends any pair candidate (it selects nothing).
            self.candidate = None;
            return GestureOutcome::PublishEmptySelection;
        };
        let completes_pair = self.candidate.is_some_and(|candidate| {
            candidate.epoch == epoch
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
            // so a triple click can never activate twice.
            self.candidate = None;
            GestureOutcome::ActivateNode(hit)
        } else {
            self.candidate = Some(Candidate {
                node: hit,
                at: now,
                world,
                epoch,
            });
            GestureOutcome::Select(hit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_graph::{NodeId, NodeKind, RenderKey};

    fn hit(id: u64) -> HitNode {
        HitNode::new(NodeId::note(id), NodeKind::Note)
    }

    /// Deterministic fake clock: a fixed reference instant plus explicit
    /// offsets — no `Instant::now` anywhere in these tests.
    fn clock(reference: Instant, millis: u64) -> Instant {
        reference + Duration::from_millis(millis)
    }

    #[test]
    fn first_click_selects_and_second_click_activates_once() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        assert_eq!(
            arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 7, 1.0),
            GestureOutcome::Select(hit(1))
        );
        assert_eq!(
            arbiter.click(clock(reference, 200), Some(hit(1)), [0.0, 0.0], 7, 1.0),
            GestureOutcome::ActivateNode(hit(1))
        );
        // Third click of a triple: fresh candidate, select only — the pair
        // was consumed, so no second activation.
        assert_eq!(
            arbiter.click(clock(reference, 300), Some(hit(1)), [0.0, 0.0], 7, 1.0),
            GestureOutcome::Select(hit(1))
        );
        assert_eq!(
            arbiter.click(clock(reference, 400), Some(hit(1)), [0.0, 0.0], 7, 1.0),
            GestureOutcome::ActivateNode(hit(1)),
            "the next pair activates again — each gesture is one intent"
        );
    }

    #[test]
    fn triple_click_after_a_consumed_pair_starts_a_new_candidate() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        assert_eq!(
            arbiter.click(clock(reference, 100), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::ActivateNode(hit(1))
        );
        // The rapid third and fourth clicks of one physical triple-click
        // never produce a second activation: the third starts a candidate
        // that is still inside the interval, so the fourth would complete
        // it — but a third click on a *consumed* pair starts a fresh
        // candidate whose second release is a deliberate gesture.
        assert_eq!(
            arbiter.click(clock(reference, 110), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::Select(hit(1))
        );
        assert_eq!(
            arbiter.click(clock(reference, 120), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::ActivateNode(hit(1)),
            "a deliberately new pair still activates"
        );
    }

    #[test]
    fn interval_boundary_is_inclusive_and_the_next_ms_rejects() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::with_config(GestureConfig::default());
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        assert_eq!(
            arbiter.click(clock(reference, 500), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::ActivateNode(hit(1)),
            "500 ms inclusive"
        );
        let mut second = GestureArbiter::with_config(GestureConfig::default());
        let _ = second.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        assert_eq!(
            second.click(clock(reference, 501), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::Select(hit(1)),
            "501 ms starts a new candidate: the geometry hold expired"
        );
    }

    #[test]
    fn distance_boundary_is_in_logical_pixels_through_zoom() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 2.0);
        // 2.0 world units at zoom 2.0 = 4 logical px = at the limit.
        assert_eq!(
            arbiter.click(clock(reference, 100), Some(hit(1)), [2.0, 0.0], 1, 2.0),
            GestureOutcome::ActivateNode(hit(1))
        );
        let mut drifted = GestureArbiter::new();
        let _ = drifted.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 2.0);
        assert_eq!(
            drifted.click(clock(reference, 100), Some(hit(1)), [2.6, 0.0], 1, 2.0),
            GestureOutcome::Select(hit(1)),
            "5.2 logical px moved: a new candidate, not the same click"
        );
    }

    #[test]
    fn other_node_never_completes_a_pair() {
        let reference = Instant::now();
        let mut other_node = GestureArbiter::new();
        let _ = other_node.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        assert_eq!(
            other_node.click(clock(reference, 10), Some(hit(2)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::Select(hit(2)),
            "double-click another node selects it (its own pair comes later)"
        );
    }

    /// S04/D-02: the navigation pair survives layout and overlay generation
    /// movement inside the hold. Only the identity epoch cancels.
    #[test]
    fn generation_movement_keeps_the_pair_but_a_new_epoch_cancels() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 5, 1.0);
        // The renderer's RenderKey generation moved (layout tick, overlay
        // revision): the pair is untouched.
        assert_eq!(
            arbiter.click(clock(reference, 100), Some(hit(1)), [0.0, 0.0], 5, 1.0),
            GestureOutcome::ActivateNode(hit(1)),
            "generation movement inside the hold is not a pair cancellation"
        );

        // A newer identity epoch cancels rather than acting on stale
        // entities: the pending candidate is discarded.
        let mut stale_epoch = GestureArbiter::new();
        let _ = stale_epoch.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 5, 1.0);
        assert!(stale_epoch.invalidate_epoch(6), "epoch change cancels");
        assert!(!stale_epoch.has_candidate());
        assert_eq!(
            stale_epoch.click(clock(reference, 10), Some(hit(1)), [0.0, 0.0], 6, 1.0),
            GestureOutcome::Select(hit(1)),
            "the post-reset click is a fresh selection, never an activation"
        );
        // Same-epoch invalidation keeps the candidate.
        let mut same = GestureArbiter::new();
        let _ = same.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 3, 1.0);
        assert!(!same.invalidate_epoch(3));
        assert!(same.has_candidate());
        assert_eq!(same.candidate_epoch(), Some(3));
    }

    #[test]
    fn background_click_publishes_empty_and_breaks_pairs() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        assert_eq!(
            arbiter.click(clock(reference, 0), None, [0.0, 0.0], 1, 1.0),
            GestureOutcome::PublishEmptySelection
        );
        let _ = arbiter.click(clock(reference, 10), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        let _ = arbiter.click(clock(reference, 20), None, [0.0, 0.0], 1, 1.0);
        assert!(!arbiter.has_candidate(), "a background click ends the pair");
    }

    #[test]
    fn cancel_clears_without_firing() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        arbiter.cancel();
        assert!(!arbiter.has_candidate());
        assert_eq!(
            arbiter.click(clock(reference, 10), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::Select(hit(1))
        );
    }

    /// D-02: the bounded hold. A candidate whose interval elapsed is
    /// expired (releasing any frozen presentation); a fresh one is kept.
    #[test]
    fn expire_drops_only_the_bounded_hold() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        // No candidate yet: expiry is a no-op.
        assert!(!arbiter.expire(clock(reference, 0)));
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        assert!(arbiter.has_candidate());
        // Inside the window the hold stays: the pair may still complete.
        assert!(!arbiter.expire(clock(reference, 499)));
        assert!(arbiter.has_candidate());
        // Past the bounded interval the second release never arrived: the
        // hold is dropped.
        assert!(arbiter.expire(clock(reference, 501)));
        assert!(!arbiter.has_candidate());
        // A new click after expiry starts a fresh candidate again.
        assert_eq!(
            arbiter.click(clock(reference, 600), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::Select(hit(1))
        );
        assert!(arbiter.has_candidate());
    }

    #[test]
    fn injected_interval_config_changes_the_boundary() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::with_config(GestureConfig {
            double_click_interval: Duration::from_millis(100),
            double_click_distance: 4.0,
        });
        let _ = arbiter.click(clock(reference, 0), Some(hit(1)), [0.0, 0.0], 1, 1.0);
        assert_eq!(
            arbiter.click(clock(reference, 150), Some(hit(1)), [0.0, 0.0], 1, 1.0),
            GestureOutcome::Select(hit(1)),
            "a 100 ms preference window rejects a 150 ms second click"
        );
    }

    /// The old blanket invalidation contract must stay dead: a changed
    /// frame generation inside the hold completes the pair (the S04 gate
    /// removes `RenderKey` from navigation pairing).
    #[test]
    fn render_key_generations_are_not_pair_authority() {
        let reference = Instant::now();
        let mut arbiter = GestureArbiter::new();
        let key_a = RenderKey::new(1, 10, 1);
        let key_b = RenderKey::new(1, 11, 1);
        assert_ne!(key_a, key_b, "the fixture really crosses generations");
        let _ = arbiter.click(
            clock(reference, 0),
            Some(hit(1)),
            [0.0, 0.0],
            key_a.epoch,
            1.0,
        );
        assert_eq!(
            arbiter.click(
                clock(reference, 50),
                Some(hit(1)),
                [0.0, 0.0],
                key_b.epoch,
                1.0
            ),
            GestureOutcome::ActivateNode(hit(1))
        );
    }
}
