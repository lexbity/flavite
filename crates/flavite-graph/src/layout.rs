//! Barnes–Hut **ForceAtlas2** force-directed layout (PLAN-B B3, D15).
//!
//! A force-directed layout is a simulation: every node is a body that is
//! *repulsed* by every other body, *attracted* along its edges, and *pulled*
//! toward the centre of the map, and the map is the configuration where those
//! forces balance. The force model implemented here follows the ForceAtlas2
//! scheme of Jacobs, Lang & Blanzé *("ForceAtlas2, a Graph Layout Algorithm
//! for Software Visualization", 2014)* — degree-weighted bodies, a
//! Barnes–Hut approximation of the all-pairs repulsion, and the adaptive
//! per-node speed with swinging damping that makes the scheme settle instead
//! of oscillate. It is an independent implementation: the formulas below are
//! the specification, derived from the paper, and no third-party source was
//! copied.
//!
//! # The forces
//!
//! Every node `i` carries a **mass** `m_i = 1 + degree_i` — a hub is a heavy
//! body, which is what keeps a knowledge graph's hubs in the middle and its
//! leaves on the periphery instead of the other way round.
//!
//! 1. **Repulsion** (Barnes–Hut approximated, quadratic in the masses):
//!    ```text
//!    F(i ← j) = k_repulsion · m_i · m_j / d²        away from j
//!    ```
//!    The `n²` pair sum is the expensive part of any layout, so it is
//!    approximated with a quadtree: a region whose angular size `s/d` falls
//!    below [`LayoutParams::theta`] is treated as one body of the region's
//!    total mass at its centre of mass. That is the *only* approximation in
//!    the model, and it is controlled by a single parameter.
//!
//! 2. **Attraction** (along the symmetrized adjacency, linear spring,
//!    hub-dissuaded):
//!    ```text
//!    F(i ← j) = k_attraction · d / (1 + degree_i)    toward j
//!    ```
//!    A linear spring — far neighbours pull proportionally harder, which is
//!    what lets a disconnected graph fold together. The `1/(1 + degree_i)`
//!    factor *dissuades hubs*: a hub of degree 1000 does not drag every leaf
//!    onto itself, it is pulled by each neighbour only weakly. On a
//!    preferentially-attached graph the unweighted variant collapses the
//!    periphery into the hub; this one does not.
//!
//! 3. **Gravity** (toward the centroid, constant magnitude):
//!    ```text
//!    F(i ← centroid) = k_gravity · m_i               toward the centroid
//!    ```
//!    Constant magnitude, not a spring: gravity is the confinement term. A
//!    body that drifts far out is pulled back at full strength rather than
//!    ever more weakly, so the map cannot disperse however long the layout
//!    runs, and a heavy hub is pulled harder than a leaf.
//!
//! # Adaptive speed and cooling
//!
//! The three forces give a *displacement* `d_i`; moving a node by `d_i`
//! directly oscillates (a spring pulled past its equilibrium pulls back past
//! it in turn). ForceAtlas2 instead moves each node by a fraction of its
//! displacement that depends on how much that node is *swinging*:
//!
//! ```text
//! swinging_i   = m_i · |d_i_prev − d_i|               (oscillation)
//! traction_i   = m_i · |d_i_prev + d_i| / 2           (directional pull)
//! speed_i      = global_speed · m_i / (1 + √m_i)      (heavier moves less)
//! factor_i     = speed_i / (1 + √(speed_i · swinging_i))
//! position_i  += factor_i · d_i
//! ```
//!
//! The global speed chases the target `jitter_tolerance · efficiency ·
//! traction / swinging`: a node that keeps reversing (large swinging) is
//! slowed down, one that pulls consistently (large traction) is sped up, and
//! the efficiency term adapts to the graph's overall jitter. On top of that,
//! [`LayoutParams::cooling_rate`] decays the speed cap each iteration — an
//! explicit cooling schedule, so that a long run keeps settling rather than
//! re-energizing.
//!
//! # Determinism
//!
//! The force pass is data-parallel — every node's displacement is a pure
//! function of the positions and the tree, so rayon may schedule it in any
//! order with no effect on the result. The one *reduction* in the step (the
//! global speed) is accumulated sequentially in index order, so a `Layout` is
//! bit-for-bit reproducible **regardless of the rayon pool size**, and a
//! layout run on 1 thread and on 15 threads produces the same positions.
//! Jitter and the co-location nudge are drawn from a stateless hash of the
//! node id, never from call order.
//!
//! # Termination
//!
//! Gravity confines the map, the speed cap bounds each step, and
//! [`LayoutParams::convergence_epsilon`] plus
//! [`LayoutParams::convergence_steps`] turn "nothing is moving any more" into
//! an explicit freeze ([`Layout::is_converged`]) that the worker turns into
//! [`LayoutState::Frozen`]. No configuration can produce a non-finite or
//! unbounded coordinate: co-located bodies are separated by a deterministic
//! nudge, non-finite intermediates are discarded, and a runaway coordinate is
//! projected back onto [`POSITION_LIMIT`].
//!
//! [`LayoutState::Frozen`]: crate::position::LayoutState

use std::collections::HashMap;

use rayon::ThreadPool;
use rayon::prelude::*;

use crate::delta::{NodeId, NodeKind};
use crate::model::GraphSnapshot;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Barnes–Hut opening angle `θ` (D15: θ ≈ 0.8).
///
/// A region is approximated as a single body when `region_side / distance <
/// θ`. Smaller is more accurate and slower: `θ = 0` is the exact `n²` sum,
/// `θ = 1` is the loosest approximation that still looks like a quadtree.
pub const DEFAULT_THETA: f64 = 0.8;

/// Smallest distance any two bodies are allowed to act across, in world
/// units. Below it, the `1/d²` repulsion would dominate every other force and
/// the direction would be undefined, so co-located bodies are separated by
/// [`nudge`] instead.
pub const MIN_SEPARATION: f64 = 1.0e-3;

/// [`MIN_SEPARATION`] squared, precomputed at the one place it is needed.
const MIN_SEPARATION_2: f64 = MIN_SEPARATION * MIN_SEPARATION;

/// Hard bound on any published coordinate.
///
/// Gravity already confines the map (§"Gravity" above), so this is never
/// reached by a healthy run; it exists so that a pathological parameter set
/// or a corrupted intermediate cannot publish a `1e308` world coordinate to
/// the renderer.
pub const POSITION_LIMIT: f64 = 1.0e9;

/// Depth cap of the quadtree.
///
/// Recursion stops here even when bodies are exactly co-located (their
/// sub-boxes collapse to zero size and would otherwise subdivide forever);
/// the bodies below the cap are lumped into one region body. 64 levels of
/// `f64` subdivision is far below the resolution of any coordinate the
/// layout ever produces, so the cap is unreachable for distinct points.
pub const MAX_TREE_DEPTH: u32 = 64;

/// Consecutive below-epsilon steps required before the layout declares itself
/// frozen. One step can be small by coincidence (a node crossing its
/// equilibrium); three in a row cannot.
pub const CONVERGENCE_STEPS: u32 = 3;

/// Rotation between successive phyllotaxis placements of a cold start — the
/// golden angle, `π(3 − √5)`. Shared with the B2 fixture generator so a probe
/// that hands the layout its own initial disc sees the same convention.
const GOLDEN_ANGLE: f64 = 2.399_963_2;

/// World radius scale of a cold start: node `i` of `n` lands at
/// `COLD_START_SCALE · √i`, i.e. a 1-node-per-unit disc.
const COLD_START_SCALE: f64 = 6.0;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Tunables of the force model. Every field is a physical knob; see the
/// module documentation for the formula each one scales.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutParams {
    /// Barnes–Hut opening angle (see [`DEFAULT_THETA`]).
    pub theta: f64,
    /// Repulsion strength `k_repulsion`.
    pub repulsion: f64,
    /// Attraction strength `k_attraction`.
    pub attraction: f64,
    /// Gravity strength `k_gravity`.
    pub gravity: f64,
    /// How jumpy the layout may be: the numerator of the adaptive-speed
    /// target. Larger tolerates more oscillation per step (faster, looser).
    pub jitter_tolerance: f64,
    /// Global speed a fresh step starts from.
    pub initial_speed: f64,
    /// Speed cap at iteration 0 — the ceiling the cooling schedule decays.
    pub speed_cap: f64,
    /// Floor of the cooling schedule: the cap never decays below this.
    pub min_speed_cap: f64,
    /// Cooling rate: `cap(iteration) = speed_cap / (1 + iteration · rate)`.
    pub cooling_rate: f64,
    /// Lower bound of the adaptive speed efficiency.
    pub speed_efficiency_min: f64,
    /// Upper bound of the adaptive speed efficiency.
    pub speed_efficiency_max: f64,
    /// Mean applied displacement (world units) below which a step counts as
    /// stationary.
    pub convergence_epsilon: f64,
    /// Largest share of the layout's mean radius any body may move in one
    /// step.
    ///
    /// The integrator is explicit Euler: `position += force · speed`. A
    /// restoring force sampled far from equilibrium overshoots, and the
    /// overshoot *grows* whenever the effective step exceeds `2/k` for the
    /// local stiffness `k`. Since both the force and the radius scale with
    /// the layout, bounding the step as a fraction of the radius is exactly
    /// the stability condition — and it costs nothing near equilibrium,
    /// where the bound does not bind.
    pub max_step_fraction: f64,
    /// Stationary steps required to freeze (see [`CONVERGENCE_STEPS`]).
    pub convergence_steps: u32,
    /// Magnitude of the warm-start jitter, in world units.
    pub jitter: f64,
    /// Seed of every hash-drawn random direction (jitter, co-location
    /// nudge). Fixed at spawn, so a run is reproducible.
    pub seed: u64,
    /// Quadtree depth cap (see [`MAX_TREE_DEPTH`]).
    pub max_tree_depth: u32,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            theta: DEFAULT_THETA,
            repulsion: 2.0,
            attraction: 1.0,
            gravity: 1.0,
            jitter_tolerance: 1.0,
            initial_speed: 1.0,
            speed_cap: 10.0,
            min_speed_cap: 0.05,
            cooling_rate: 0.02,
            speed_efficiency_min: 0.05,
            speed_efficiency_max: 1.0,
            convergence_epsilon: 1.0e-4,
            max_step_fraction: 0.1,
            convergence_steps: CONVERGENCE_STEPS,
            jitter: 1.0,
            seed: 0x5EED_C0DE,
            max_tree_depth: MAX_TREE_DEPTH,
        }
    }
}

impl LayoutParams {
    /// Clamp every field into a range the force model is defined on.
    ///
    /// Parameters cross a thread boundary (the worker is configured from the
    /// UI), so junk — a negative mass scale, a `NaN` tolerance, a `θ` of 4 —
    /// is *rejected* rather than propagated into the arithmetic. Each field
    /// falls back to the documented default; nothing else changes.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let positive = |v: f64, fallback: f64| if v.is_finite() && v > 0.0 { v } else { fallback };
        Self {
            theta: positive(self.theta, d.theta).clamp(0.05, 1.5),
            repulsion: positive(self.repulsion, d.repulsion),
            attraction: positive(self.attraction, d.attraction),
            gravity: positive(self.gravity, d.gravity),
            jitter_tolerance: positive(self.jitter_tolerance, d.jitter_tolerance),
            initial_speed: positive(self.initial_speed, d.initial_speed),
            speed_cap: positive(self.speed_cap, d.speed_cap),
            min_speed_cap: positive(self.min_speed_cap, d.min_speed_cap),
            cooling_rate: positive(self.cooling_rate, d.cooling_rate),
            speed_efficiency_min: positive(self.speed_efficiency_min, d.speed_efficiency_min)
                .min(1.0),
            speed_efficiency_max: positive(self.speed_efficiency_max, d.speed_efficiency_max)
                .clamp(0.05, 1.0),
            convergence_epsilon: positive(self.convergence_epsilon, d.convergence_epsilon),
            max_step_fraction: positive(self.max_step_fraction, d.max_step_fraction)
                .clamp(1.0e-3, 1.0),
            convergence_steps: self.convergence_steps.clamp(1, 64),
            jitter: if self.jitter.is_finite() && self.jitter >= 0.0 {
                self.jitter
            } else {
                d.jitter
            },
            seed: self.seed,
            max_tree_depth: self.max_tree_depth.clamp(1, MAX_TREE_DEPTH),
        }
    }
}

// ---------------------------------------------------------------------------
// Stateless hashing (jitter and nudge directions)
// ---------------------------------------------------------------------------

/// SplitMix64 finalizer: a stateless integer hash.
///
/// Used instead of a seeded PRNG stream so that the jitter of a node depends
/// only on `(seed, node id)` — re-running a topology change reproduces the
/// same warm start, and the result never depends on the order in which nodes
/// happen to be visited.
const fn mix(seed: u64, value: u64) -> u64 {
    let mut z = seed ^ value.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A hash-drawn value in `[0, 1)` from two integers.
///
/// 24 mantissa bits — exactly representable in `f64`, and far more resolution
/// than a jitter offset or a nudge angle needs.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "hash bits are truncated on purpose; 24 mantissa bits suffice"
)]
fn hash_unit(seed: u64, a: u64, b: u64) -> f64 {
    let bits = (mix(seed, mix(a, b)) >> 40) as u32;
    f64::from(bits) / f64::from(1u32 << 24)
}

/// A hash-drawn direction in `[0, 2π)` from two integers.
fn hash_angle(seed: u64, a: u64, b: u64) -> f64 {
    2.0 * std::f64::consts::PI * hash_unit(seed, a ^ 0xA5A5_A5A5, b)
}

/// A deterministic jitter offset in `[-jitter, jitter]²` for a node.
fn jitter_offset(seed: u64, node: u64, jitter: f64) -> [f64; 2] {
    [
        (hash_unit(seed, node, 0x1111) - 0.5) * 2.0 * jitter,
        (hash_unit(seed, node, 0x2222) - 0.5) * 2.0 * jitter,
    ]
}

// ---------------------------------------------------------------------------
// Barnes–Hut quadtree
// ---------------------------------------------------------------------------

/// Marker for "this region node is not a single-body leaf".
const NO_BODY: u32 = u32::MAX;
/// Marker for "this region node has no child in that quadrant".
const NO_CHILD: u32 = u32::MAX;

/// One region of the quadtree, stored in an arena ([`QuadTree::nodes`]).
#[derive(Debug, Clone, Copy)]
struct Region {
    /// Box centre.
    cx: f64,
    cy: f64,
    /// Half of the box side length.
    half: f64,
    /// Mass-weighted position sum over the subtree — divide by
    /// [`Region::mass`] for the centre of mass.
    com_x: f64,
    com_y: f64,
    /// Total mass of the subtree.
    mass: f64,
    /// Bodies in the subtree (0 only for an untouched root).
    bodies: u32,
    /// The single body of a leaf, or [`NO_BODY`] for a region/bucket.
    body: u32,
    /// Child arena indices, or [`NO_CHILD`].
    children: [u32; 4],
}

impl Region {
    fn empty(cx: f64, cy: f64, half: f64) -> Self {
        Self {
            cx,
            cy,
            half,
            com_x: 0.0,
            com_y: 0.0,
            mass: 0.0,
            bodies: 0,
            body: NO_BODY,
            children: [NO_CHILD; 4],
        }
    }

    /// Whether this region holds exactly one body (a leaf).
    fn is_single_body(&self) -> bool {
        self.body != NO_BODY
    }

    /// Whether this region has been subdivided.
    fn is_subdivided(&self) -> bool {
        self.children[0] != NO_CHILD
    }

    /// Side length of the box.
    fn side(&self) -> f64 {
        self.half * 2.0
    }

    /// The centre of mass of the subtree (only meaningful when `mass > 0`).
    fn centre_of_mass(&self) -> (f64, f64) {
        (self.com_x / self.mass, self.com_y / self.mass)
    }

    /// Quadrant of a point relative to the box `(cx, cy)`: 0 SW, 1 SE, 2 NW,
    /// 3 NE. A free function rather than a method so a caller that already
    /// has the box coordinates does not have to build a throwaway region.
    fn quadrant_of(cx: f64, cy: f64, x: f64, y: f64) -> usize {
        usize::from(x > cx) + 2 * usize::from(y > cy)
    }
}

/// A quadtree over the current positions, built once per step and queried
/// once per node.
#[derive(Debug, Clone)]
struct QuadTree {
    regions: Vec<Region>,
}

impl QuadTree {
    /// Build the tree over `positions`, weighting each body by `masses`.
    fn build(positions: &[[f64; 2]], masses: &[f64], max_depth: u32) -> Self {
        let node_count = positions.len();
        debug_assert_eq!(masses.len(), node_count);

        // Square bounding box with a small margin, so that a degenerate
        // (single point, or all-collinear) layout still has a box to split.
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for [x, y] in positions {
            min_x = min_x.min(*x);
            min_y = min_y.min(*y);
            max_x = max_x.max(*x);
            max_y = max_y.max(*y);
        }
        let centre_x = (min_x + max_x) * 0.5;
        let centre_y = (min_y + max_y) * 0.5;
        let half = ((max_x - min_x).max(max_y - min_y)) * 0.5 * 1.05 + 1.0;

        let mut tree = Self {
            regions: Vec::with_capacity(node_count.next_power_of_two().min(1 << 20)),
        };
        tree.regions.push(Region::empty(centre_x, centre_y, half));
        if node_count == 0 {
            return tree;
        }
        for body in 0..node_count {
            tree.insert(0, body, positions, masses, 0, max_depth);
        }
        tree
    }

    /// Insert `body` into the subtree rooted at `region`, accumulating its
    /// mass along the path.
    fn insert(
        &mut self,
        region: u32,
        body: usize,
        positions: &[[f64; 2]],
        masses: &[f64],
        depth: u32,
        max_depth: u32,
    ) {
        let [x, y] = positions[body];
        let mass = masses[body];
        let index = region as usize;

        // Accumulate exactly once per body per region on the path: a body's
        // mass lands in every region that contains it, and in no other.
        self.regions[index].mass += mass;
        self.regions[index].com_x += x * mass;
        self.regions[index].com_y += y * mass;
        self.regions[index].bodies += 1;

        let (cx, cy, half) = {
            let r = &self.regions[index];
            (r.cx, r.cy, r.half)
        };

        if self.regions[index].is_single_body() {
            // A second body arrived in a single-body leaf: subdivide, then
            // send both bodies down one level. The parent's accumulation is
            // already final, so only the children accumulate again.
            let displaced = self.regions[index].body as usize;
            let quadrant = Region::quadrant_of(cx, cy, positions[displaced][0], positions[displaced][1]);
            let children = self.subdivide(index, cx, cy, half);
            self.regions[index].body = NO_BODY;
            let displaced_child = children[quadrant];
            let incoming = children[Region::quadrant_of(cx, cy, x, y)];
            self.insert(displaced_child, displaced, positions, masses, depth + 1, max_depth);
            self.insert(incoming, body, positions, masses, depth + 1, max_depth);
            return;
        }

        if !self.regions[index].is_subdivided() {
            if depth >= max_depth {
                // Depth cap: several bodies share this region and it will not
                // be split any further. They are approximated together as one
                // body of the region's total mass (see [`MAX_TREE_DEPTH`]).
                return;
            }
            // An untouched region becomes a single-body leaf.
            self.regions[index].body = body as u32;
            return;
        }

        let quadrant = Region::quadrant_of(cx, cy, x, y);
        let child = self.regions[index].children[quadrant];
        self.insert(child, body, positions, masses, depth + 1, max_depth);
    }

    /// Create the four quadrants of region `index` and return their arena
    /// indices (SW, SE, NW, NE).
    fn subdivide(&mut self, index: usize, cx: f64, cy: f64, half: f64) -> [u32; 4] {
        let child_half = half * 0.5;
        let mut children = [NO_CHILD; 4];
        for quadrant in 0..4 {
            let offset_x = if quadrant % 2 == 0 { -child_half } else { child_half };
            let offset_y = if quadrant < 2 { -child_half } else { child_half };
            let child = self.regions.len() as u32;
            self.regions.push(Region::empty(
                cx + offset_x,
                cy + offset_y,
                child_half,
            ));
            children[quadrant] = child;
        }
        self.regions[index].children = children;
        children
    }

    /// Accumulate the net repulsion of the whole tree on body `i` into
    /// `out`.
    fn repulsion_into(
        &self,
        positions: &[[f64; 2]],
        masses: &[f64],
        i: usize,
        theta_2: f64,
        coefficient: f64,
        out: &mut [f64; 2],
    ) {
        self.visit_repulsion(0, positions, masses, i, theta_2, coefficient, out);
    }

    /// One node of the Barnes–Hut traversal.
    ///
    /// The recursion is depth-capped by the tree itself, so the stack can
    /// never exceed `max_tree_depth` frames — no explicit traversal stack,
    /// and therefore no allocation on the force path.
    fn visit_repulsion(
        &self,
        region: u32,
        positions: &[[f64; 2]],
        masses: &[f64],
        i: usize,
        theta_2: f64,
        coefficient: f64,
        out: &mut [f64; 2],
    ) {
        let r = &self.regions[region as usize];
        if r.bodies == 0 {
            return;
        }
        let (com_x, com_y) = r.centre_of_mass();
        // From the query body *away* from the region: this is repulsion, so
        // the vector must point out of the region, not into it. Getting this
        // backwards turns the whole model into an extra gravity well and the
        // map collapses.
        let dx = positions[i][0] - com_x;
        let dy = positions[i][1] - com_y;
        let d_2 = dx * dx + dy * dy;

        if r.is_single_body() {
            let j = r.body as usize;
            if j == i {
                // No self-repulsion.
                return;
            }
            debug_assert_eq!(r.mass, masses[j]);
            add_pair_repulsion(dx, dy, d_2, coefficient * masses[i] * masses[j], i, region, out);
            return;
        }

        if !r.is_subdivided() {
            // A bucket of bodies at the depth cap. They are (at most) as
            // spread as the cap allows, and if the query body sits on their
            // centre of mass the pair force has no direction — separate them
            // deterministically instead.
            add_pair_repulsion(
                dx,
                dy,
                d_2,
                coefficient * masses[i] * r.mass,
                i,
                region,
                out,
            );
            return;
        }

        // Barnes–Hut criterion: approximate when the region subtends an
        // angle below θ, i.e. (side/d)² < θ².
        if d_2 > 0.0 && r.side() * r.side() / d_2 < theta_2 {
            add_pair_repulsion(
                dx,
                dy,
                d_2,
                coefficient * masses[i] * r.mass,
                i,
                region,
                out,
            );
            return;
        }

        for child in r.children {
            if child != NO_CHILD {
                self.visit_repulsion(child, positions, masses, i, theta_2, coefficient, out);
            }
        }
    }
}

/// Apply one pair repulsion `coefficient / d²` along `(dx, dy)`.
///
/// The magnitude is `coefficient / d²` and the direction is the unit vector
/// along `(dx, dy)`, so the contribution is `(dx, dy) · coefficient / d³`.
/// Exactly co-located bodies (`d² = 0`) have no direction to share: they are
/// separated by a deterministic, isotropic nudge of the same magnitude at
/// [`MIN_SEPARATION`], which keeps the step finite and the run reproducible.
fn add_pair_repulsion(
    dx: f64,
    dy: f64,
    d_2: f64,
    coefficient: f64,
    i: usize,
    region: u32,
    out: &mut [f64; 2],
) {
    if d_2 >= MIN_SEPARATION_2 {
        let scale = coefficient / (d_2 * d_2.sqrt());
        out[0] += dx * scale;
        out[1] += dy * scale;
    } else {
        let nudge = nudge(coefficient, i, region);
        out[0] += nudge[0];
        out[1] += nudge[1];
    }
}

/// A deterministic separation vector for bodies that occupy the same point.
///
/// Magnitude is the pair force evaluated at [`MIN_SEPARATION`] (capped so it
/// cannot dominate the step), direction is drawn from a hash of
/// `(body, region)` so two co-located bodies never share one — and so the
/// separation is identical on every run and on every thread count.
fn nudge(coefficient: f64, body: usize, region: u32) -> [f64; 2] {
    const SEED: u64 = 0x9E37_79B9;
    let angle = hash_angle(SEED, body as u64, u64::from(region));
    let magnitude = (coefficient / MIN_SEPARATION_2).min(1.0e6);
    [angle.cos() * magnitude, angle.sin() * magnitude]
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// What one [`Layout::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StepStats {
    /// Iteration counter after this step (monotone; restarts on a topology
    /// change, which restarts the cooling schedule).
    pub iteration: u64,
    /// Global speed after the adaptive update and the cooling cap.
    pub global_speed: f64,
    /// Adaptive speed efficiency after this step.
    pub speed_efficiency: f64,
    /// Total swinging per unit mass — the oscillation measure.
    pub swinging_per_mass: f64,
    /// Total traction per unit mass — the directional-pull measure.
    pub traction_per_mass: f64,
    /// Largest applied displacement of any node, in world units.
    pub max_displacement: f64,
    /// Mean applied displacement over all nodes, in world units.
    pub mean_displacement: f64,
    /// Largest raw force magnitude of any node, before the speed factor.
    ///
    /// Diagnostic: a force orders of magnitude above the layout's own scale
    /// means two bodies are sitting on top of each other.
    pub max_force: f64,
    /// Positions projected back onto [`POSITION_LIMIT`] this step (0 in any
    /// healthy run).
    pub diverged: u64,
    /// Whether this step met the convergence epsilon.
    pub stationary: bool,
    /// Whether convergence has held for [`LayoutParams::convergence_steps`]
    /// steps — the layout is frozen from here on.
    pub frozen: bool,
}

/// The result of one step: the displacement actually applied to each node,
/// plus [`StepStats`].
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Applied displacement per node, in dense index order. `positions[i] +=
    /// displacements[i]` is exactly what the step did.
    pub displacements: Vec<[f64; 2]>,
    /// Aggregate statistics of the step.
    pub stats: StepStats,
}

/// What [`Layout::retarget`] did when the topology changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetargetStats {
    /// Nodes that existed before and kept their position.
    pub kept: usize,
    /// Nodes placed from scratch: a cold start (first install) or a warm
    /// start at their neighbours' centroid + jitter.
    pub placed: usize,
    /// Nodes that disappeared with the old topology.
    pub removed: usize,
}

impl RetargetStats {
    /// Whether the topology gained or lost anything.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.placed > 0 || self.removed > 0
    }
}

// ---------------------------------------------------------------------------
// The layout
// ---------------------------------------------------------------------------

/// A Barnes–Hut ForceAtlas2 layout over a [`GraphSnapshot`].
///
/// The layout owns the topology it lays out (a copy of the snapshot's
/// symmetrized CSR plus the dense `NodeId` order) and the whole simulation
/// state, so a step needs nothing but a rayon pool:
///
/// ```text
/// let mut layout = Layout::from_snapshot(&snapshot, LayoutParams::default());
/// let step = layout.step(&pool);
/// let positions = layout.positions();       // f64, dense order
/// ```
///
/// Topology changes go through [`Layout::retarget`], which keeps every
/// surviving node's position and warm-starts the newcomers — the layout never
/// restarts from scratch because a note was added.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Dense `NodeId` order of the snapshot this layout was retargeted to.
    node_ids: Vec<NodeId>,
    /// Symmetrized degree per dense index.
    degrees: Vec<u32>,
    /// `1 + degree` per dense index — the body mass.
    masses: Vec<f64>,
    /// Symmetrized adjacency, CSR ([`GraphSnapshot::adj_offsets`]).
    adj_offsets: Vec<u32>,
    /// Symmetrized adjacency, CSR ([`GraphSnapshot::adj_indices`]).
    adj_indices: Vec<u32>,
    /// Current positions, dense order.
    positions: Vec<[f64; 2]>,
    /// Raw displacement of the previous step (before the speed factor) —
    /// the second half of the swinging/traction measure.
    displacement: Vec<[f64; 2]>,
    /// Scratch for the raw displacement of the current step.
    raw: Vec<[f64; 2]>,
    /// Per-node swinging of the previous step.
    swinging: Vec<f64>,
    /// Global speed after the last adaptive update.
    global_speed: f64,
    /// Adaptive speed efficiency.
    speed_efficiency: f64,
    /// Steps taken against the current topology.
    iteration: u64,
    /// Iterations since the last wake, in `f64` — the cooling schedule's
    /// age. Kept beside [`Layout::iteration`] because the cooling arithmetic
    /// is floating point and must not round-trip through an integer cast.
    cooling_age: f64,
    /// Consecutive stationary steps.
    converged_steps: u32,
    /// Sanitized tunables.
    params: LayoutParams,
}

impl Default for Layout {
    fn default() -> Self {
        Self::new(LayoutParams::default())
    }
}

impl Layout {
    /// An empty layout, ready to be [`Self::retarget`]ed to a snapshot.
    #[must_use]
    pub fn new(params: LayoutParams) -> Self {
        let params = params.sanitized();
        Self {
            node_ids: Vec::new(),
            degrees: Vec::new(),
            masses: Vec::new(),
            adj_offsets: Vec::new(),
            adj_indices: Vec::new(),
            positions: Vec::new(),
            displacement: Vec::new(),
            raw: Vec::new(),
            swinging: Vec::new(),
            global_speed: params.initial_speed,
            speed_efficiency: 1.0,
            iteration: 0,
            cooling_age: 0.0,
            converged_steps: 0,
            params,
        }
    }

    /// A layout positioned at `snapshot` (cold start, see
    /// [`Layout::retarget`]).
    #[must_use]
    pub fn from_snapshot(snapshot: &GraphSnapshot, params: LayoutParams) -> Self {
        let mut layout = Self::new(params);
        layout.retarget(snapshot);
        layout
    }

    /// The tunables this layout runs with (already sanitized).
    #[must_use]
    pub fn params(&self) -> LayoutParams {
        self.params
    }

    /// Nodes currently laid out.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.positions.len()
    }

    /// Positions in dense index order, in world units.
    #[must_use]
    pub fn positions(&self) -> &[[f64; 2]] {
        &self.positions
    }

    /// The dense `NodeId` order positions are indexed by.
    #[must_use]
    pub fn node_ids(&self) -> &[NodeId] {
        &self.node_ids
    }

    /// Steps taken against the current topology.
    #[must_use]
    pub fn iteration(&self) -> u64 {
        self.iteration
    }

    /// Whether the layout has stopped moving ([`LayoutParams::convergence_steps`]
    /// consecutive stationary steps). A frozen layout still accepts a
    /// topology change, which wakes it.
    #[must_use]
    pub fn is_converged(&self) -> bool {
        self.converged_steps >= self.params.convergence_steps
    }

    /// Write the current positions as `f32` into `out` (the renderer's world
    /// type, and the layout's published type), resizing `out` to the node
    /// count.
    ///
    /// `f64 → f32` is a deliberate precision loss: the simulation runs in
    /// `f64` so that 50 000 accumulated steps stay stable, while the renderer
    /// works in `f32` and only ever needs [`POSITION_LIMIT`]-bounded
    /// coordinates.
    #[allow(
        clippy::cast_precision_loss,
        reason = "f64 simulation -> f32 renderer is the documented hand-off"
    )]
    pub fn write_positions_f32(&self, out: &mut Vec<[f32; 2]>) {
        out.clear();
        out.reserve(self.positions.len());
        for [x, y] in &self.positions {
            out.push([*x as f32, *y as f32]);
        }
    }

    /// Take one step of the simulation, on `pool`.
    ///
    /// Returns the applied displacement per node plus [`StepStats`]. For a
    /// steady-state caller that reuses its buffer, prefer
    /// [`Layout::step_into`].
    pub fn step(&mut self, pool: &ThreadPool) -> Step {
        let mut displacements = vec![[0.0_f64; 2]; self.node_count()];
        let stats = self.step_into(pool, &mut displacements);
        Step {
            displacements,
            stats,
        }
    }

    /// Take one step of the simulation, writing the applied displacement of
    /// each node into `displacements` (resized to the node count).
    ///
    /// The step is four passes:
    ///
    /// 1. **Forces** — one quadtree per step, then one force evaluation per
    ///    node in parallel. Order-independent: no reduction crosses nodes.
    /// 2. **Adaptive speed** — the one reduction of the step, accumulated in
    ///    index order so the result cannot depend on the pool.
    /// 3. **Apply** — each node moves by its own speed-scaled displacement.
    /// 4. **Convergence** — count stationary steps toward a freeze.
    ///
    /// # Panics
    ///
    /// Never, under the invariants [`Layout::retarget`] maintains. A caller
    /// that has handed the layout a malformed snapshot cannot get here: the
    /// worker catches the resulting panic at the boundary (§4.2).
    pub fn step_into(&mut self, pool: &ThreadPool, displacements: &mut Vec<[f64; 2]>) -> StepStats {
        let Self {
            degrees,
            masses,
            adj_offsets,
            adj_indices,
            positions,
            displacement: previous_raw,
            raw,
            swinging,
            iteration,
            converged_steps,
            params,
            cooling_age,
            ..
        } = self;
        let params = *params;
        let node_count = positions.len();

        displacements.clear();
        if node_count == 0 {
            return StepStats {
                frozen: *converged_steps >= params.convergence_steps,
                ..StepStats::default()
            };
        }
        displacements.resize(node_count, [0.0; 2]);

        // --- 1. Forces (parallel, deterministic: no cross-node reduction) --
        // The tree build is sequential — its *structure* must be identical on
        // every run, and a sequential build gives that by construction. Its
        // per-step cost is O(n log n), same order as the force pass itself.
        let tree = QuadTree::build(&*positions, &*masses, params.max_tree_depth);
        let theta_2 = params.theta * params.theta;
        let (centroid, mean_radius) = centroid_and_radius(&*positions);
        let degrees = &*degrees;
        let masses = &*masses;
        let adj_offsets = &*adj_offsets;
        let adj_indices = &*adj_indices;
        let current: &[[f64; 2]] = positions;
        let raw = &mut *raw;
        pool.install(|| {
            raw.par_iter_mut().enumerate().for_each(|(i, slot)| {
                *slot = forces_on(
                    i,
                    &tree,
                    current,
                    masses,
                    degrees,
                    adj_offsets,
                    adj_indices,
                    centroid,
                    theta_2,
                    params,
                );
            });
        });
        let raw: &[[f64; 2]] = raw;

        // --- 2. Adaptive speed (sequential: the one reduction) ------------
        let mut swinging_total = 0.0;
        let mut traction_total = 0.0;
        let mut mass_total = 0.0;
        for i in 0..node_count {
            let mass = masses[i];
            let d = raw[i];
            let previous = previous_raw[i];
            let swing = ((d[0] - previous[0]).powi(2) + (d[1] - previous[1]).powi(2)).sqrt();
            let pull = ((d[0] + previous[0]).powi(2) + (d[1] + previous[1]).powi(2)).sqrt() * 0.5;
            swinging[i] = mass * swing;
            swinging_total += swinging[i];
            traction_total += mass * pull;
            mass_total += mass;
        }
        let swinging_per_mass = swinging_total / mass_total;
        let traction_per_mass = traction_total / mass_total;

        let mut efficiency = self.speed_efficiency;
        if swinging_per_mass > params.jitter_tolerance {
            efficiency = (efficiency * 0.7).max(params.speed_efficiency_min);
        } else if swinging_per_mass < params.jitter_tolerance * 0.1 {
            efficiency = (efficiency * 1.3).min(params.speed_efficiency_max);
        }
        let target_speed =
            params.jitter_tolerance * efficiency * traction_per_mass / (swinging_per_mass + 1.0e-6);
        let mut speed = self.global_speed;
        if speed < target_speed {
            speed = (speed * 1.5).min(target_speed);
        } else {
            speed = target_speed.max(speed / 1.5);
        }
        // Cooling schedule: the speed cap decays with the iteration count
        // down to a floor, so a long run keeps settling rather than
        // re-energizing. `cooling_age` is the iteration count in world
        // arithmetic (f64), restarted by `wake`.
        let cap = (params.speed_cap / (1.0 + *cooling_age * params.cooling_rate))
            .max(params.min_speed_cap)
            .max(params.initial_speed.min(params.speed_cap));
        speed = speed.min(cap);
        self.global_speed = speed;
        self.speed_efficiency = efficiency;

        // --- 3. Per-node speed factor, then the stability cap -------------
        {
            let swinging = &*swinging;
            let displacements = &mut *displacements;
            let raw = raw;
            pool.install(|| {
                displacements
                    .par_iter_mut()
                    .zip(raw.par_iter())
                    .enumerate()
                    .for_each(|(i, (slot, raw_slot))| {
                        let mass = masses[i];
                        let node_speed = speed * mass / (1.0 + mass.sqrt());
                        let factor = node_speed / (1.0 + (node_speed * swinging[i]).sqrt());
                        let step = [raw_slot[0] * factor, raw_slot[1] * factor];
                        // Defensive: a non-finite intermediate is discarded
                        // rather than published.
                        *slot = if step[0].is_finite() && step[1].is_finite() {
                            step
                        } else {
                            [0.0; 2]
                        };
                    });
            });
        }

        // Stability cap — see `LayoutParams::max_step_fraction`. One
        // uniform factor for the whole step, so relative motion (and
        // therefore the shape of the map) is preserved exactly; a per-body
        // cap would shear it.
        if mean_radius > 0.0 {
            let budget = params.max_step_fraction * mean_radius;
            let mut fastest = 0.0_f64;
            for step in displacements.iter() {
                fastest = fastest.max(step[0].abs().max(step[1].abs()));
            }
            if fastest > budget {
                let scale = budget / fastest;
                for step in displacements.iter_mut() {
                    step[0] *= scale;
                    step[1] *= scale;
                }
            }
        }

        // Apply (parallel).
        {
            let positions = &mut *positions;
            let displacements = &*displacements;
            pool.install(|| {
                positions
                    .par_iter_mut()
                    .zip(displacements.par_iter())
                    .for_each(|(position, step)| {
                        position[0] += step[0];
                        position[1] += step[1];
                        if !position[0].is_finite() || !position[1].is_finite() {
                            *position = [0.0; 2];
                        }
                    });
            });
        }

        // --- 4. Bookkeeping, divergence guard, convergence ----------------
        let mut max_displacement = 0.0_f64;
        let mut max_force = 0.0_f64;
        let mut divergence = 0_u64;
        for (i, position) in positions.iter_mut().enumerate() {
            if position[0].abs() > POSITION_LIMIT || position[1].abs() > POSITION_LIMIT {
                position[0] = position[0].clamp(-POSITION_LIMIT, POSITION_LIMIT);
                position[1] = position[1].clamp(-POSITION_LIMIT, POSITION_LIMIT);
                divergence += 1;
            }
            previous_raw[i] = raw[i];
            let magnitude = (displacements[i][0].powi(2) + displacements[i][1].powi(2)).sqrt();
            max_displacement = max_displacement.max(magnitude);
            max_force = max_force.max(raw[i][0].abs().max(raw[i][1].abs()));
        }
        let mean_displacement = {
            let total: f64 = displacements
                .iter()
                .map(|[x, y]| (x * x + y * y).sqrt())
                .sum();
            total / node_count as f64
        };

        *iteration += 1;
        *cooling_age += 1.0;
        let stationary = mean_displacement < params.convergence_epsilon;
        *converged_steps = if stationary {
            converged_steps.saturating_add(1)
        } else {
            0
        };

        StepStats {
            iteration: *iteration,
            global_speed: self.global_speed,
            speed_efficiency: self.speed_efficiency,
            swinging_per_mass,
            traction_per_mass,
            max_displacement,
            mean_displacement,
            max_force,
            diverged: divergence,
            stationary,
            frozen: *converged_steps >= params.convergence_steps,
        }
    }

    /// Re-target the layout at a new topology, keeping every surviving node's
    /// position.
    ///
    /// - A node whose [`NodeId`] is already laid out keeps its position.
    /// - A node that is new to the layout is **warm started** at the centroid
    ///   of its already-placed neighbours plus a hash-drawn jitter offset —
    ///   a new note attached to three existing ones starts between them, not
    ///   at the origin.
    /// - A new node with no placed neighbour (a fresh component) starts at
    ///   the centroid of the placed nodes plus jitter.
    /// - The *first* install (an empty layout) instead lays every node out on
    ///   a phyllotaxis disc, so a cold start is already spread out rather
    ///   than a single point that has to be pushed apart.
    ///
    /// A retarget restarts the cooling schedule and clears the adaptive state
    /// (the topology changed, so last step's speeds and swinging no longer
    /// describe this graph). Use [`Layout::wake`] to re-energize without
    /// changing the topology.
    pub fn retarget(&mut self, snapshot: &GraphSnapshot) -> RetargetStats {
        let node_count = snapshot.node_count();
        let previous: HashMap<NodeId, usize> = self
            .node_ids
            .iter()
            .enumerate()
            .map(|(index, &id)| (id, index))
            .collect();
        let cold_start = previous.is_empty();

        let mut positions = vec![[0.0_f64; 2]; node_count];
        let mut kept = 0_usize;
        let mut placed = 0_usize;

        // Pass 1 — keep the survivors.
        for (index, &id) in snapshot.node_ids.iter().enumerate() {
            match previous.get(&id) {
                Some(&old) => {
                    positions[index] = self.positions[old];
                    kept += 1;
                }
                None => {
                    positions[index] = if cold_start {
                        cold_start_position(index)
                    } else {
                        [f64::NAN; 2] // filled in pass 2
                    };
                    placed += 1;
                }
            }
        }

        // Pass 2 — warm start the newcomers at their neighbours' centroid.
        if !cold_start && placed > 0 {
            let neighbour_centroid =
                |positions: &[[f64; 2]], index: usize| -> Option<[f64; 2]> {
                    let start = snapshot.adj_offsets[index] as usize;
                    let end = snapshot.adj_offsets[index + 1] as usize;
                    let mut sum = [0.0_f64; 2];
                    let mut count = 0_usize;
                    for row in start..end {
                        let neighbour = snapshot.adj_indices[row] as usize;
                        if neighbour < node_count && !positions[neighbour][0].is_nan() {
                            sum[0] += positions[neighbour][0];
                            sum[1] += positions[neighbour][1];
                            count += 1;
                        }
                    }
                    (count > 0).then(|| [sum[0] / count as f64, sum[1] / count as f64])
                };
            let global_centroid = {
                let mut sum = [0.0_f64; 2];
                let mut count = 0_usize;
                for position in &positions {
                    if !position[0].is_nan() {
                        sum[0] += position[0];
                        sum[1] += position[1];
                        count += 1;
                    }
                }
                if count > 0 {
                    [sum[0] / count as f64, sum[1] / count as f64]
                } else {
                    [0.0; 2]
                }
            };
            for index in 0..node_count {
                if positions[index][0].is_nan() {
                    let base = neighbour_centroid(&positions, index).unwrap_or(global_centroid);
                    let jitter = jitter_offset(
                        self.params.seed,
                        node_key(snapshot.node_ids[index]),
                        self.params.jitter,
                    );
                    positions[index] = [base[0] + jitter[0], base[1] + jitter[1]];
                }
            }
        }

        let removed = self.node_ids.len().saturating_sub(kept);

        self.node_ids = snapshot.node_ids.clone();
        self.degrees = snapshot.degrees.clone();
        self.masses = snapshot
            .degrees
            .iter()
            .map(|&degree| 1.0 + f64::from(degree))
            .collect();
        self.adj_offsets = snapshot.adj_offsets.clone();
        self.adj_indices = snapshot.adj_indices.clone();
        self.positions = positions;
        self.displacement = vec![[0.0; 2]; node_count];
        self.raw = vec![[0.0; 2]; node_count];
        self.swinging = vec![0.0; node_count];
        self.wake();

        RetargetStats {
            kept,
            placed,
            removed,
        }
    }

    /// Re-energize a frozen layout: reset the adaptive state and the cooling
    /// schedule so the next steps can move the map again.
    ///
    /// Called automatically on [`Layout::retarget`] and by the worker when a
    /// command wakes a frozen layout (D15: freeze on cool, wake on topology
    /// change).
    pub fn wake(&mut self) {
        self.global_speed = self.params.initial_speed;
        self.speed_efficiency = 1.0;
        self.iteration = 0;
        self.cooling_age = 0.0;
        self.converged_steps = 0;
        for displacement in &mut self.displacement {
            *displacement = [0.0; 2];
        }
    }
}

/// World position of node `index` on a cold start: a phyllotaxis disc, one
/// node per world unit of radius.
///
/// The same convention the B2 fixture generator uses, so a probe that hands
/// the layout a disc it can see the layout start from a comparable map
/// instead of a collapsed point.
#[allow(clippy::cast_precision_loss, reason = "index -> f64 radius, exact enough")]
fn cold_start_position(index: usize) -> [f64; 2] {
    let i = index as f64;
    let radius = COLD_START_SCALE * i.sqrt();
    let angle = i * GOLDEN_ANGLE;
    [radius * angle.cos(), radius * angle.sin()]
}

/// Centroid of `positions`, and the mean distance of a body from it.
///
/// The second half is the layout's characteristic scale: the length every
/// world coordinate in the step is measured against, and the budget
/// [`LayoutParams::max_step_fraction`] caps a single iteration's movement
/// to. Empty input is `[0, 0]` with a scale of `0`, which disables the cap
/// (there is nothing to move).
fn centroid_and_radius(positions: &[[f64; 2]]) -> ([f64; 2], f64) {
    if positions.is_empty() {
        return ([0.0_f64; 2], 0.0);
    }
    let mut sum = [0.0_f64; 2];
    for [x, y] in positions {
        sum[0] += x;
        sum[1] += y;
    }
    let count = positions.len() as f64;
    let centroid = [sum[0] / count, sum[1] / count];
    let mut radius = 0.0_f64;
    for [x, y] in positions {
        radius += (x - centroid[0]).hypot(y - centroid[1]);
    }
    (centroid, radius / count)
}

/// A stable integer key for a [`NodeId`], for hash-drawn jitter.
///
/// Deliberately not `std`'s `Hasher`: the default hasher's algorithm is
/// unspecified and may change between Rust releases, which would silently
/// change every warm start. This mix of the kind tag and the id is fixed
/// forever.
fn node_key(id: NodeId) -> u64 {
    let kind = match id.kind {
        NodeKind::Note => 0_u64,
        NodeKind::Attachment => 1_u64,
    };
    mix(kind, id.id)
}

/// The net force on node `i`: repulsion (Barnes–Hut) + attraction (springs
/// over the symmetrized adjacency) + gravity (toward the centroid).
///
/// Pure in `(tree, positions, masses, adjacency, centroid)`, which is what
/// makes the parallel force pass order-independent.
#[allow(clippy::too_many_arguments)]
fn forces_on(
    i: usize,
    tree: &QuadTree,
    positions: &[[f64; 2]],
    masses: &[f64],
    degrees: &[u32],
    adj_offsets: &[u32],
    adj_indices: &[u32],
    centroid: [f64; 2],
    theta_2: f64,
    params: LayoutParams,
) -> [f64; 2] {
    let [x, y] = positions[i];
    let mut out = [0.0_f64; 2];

    // 1. Repulsion — all other bodies, Barnes–Hut approximated.
    tree.repulsion_into(positions, masses, i, theta_2, params.repulsion, &mut out);

    // 2. Attraction — one linear spring per symmetrized neighbour, scaled by
    //    1/(1 + degree) so a hub is not dragged onto its leaves.
    let start = adj_offsets[i] as usize;
    let end = adj_offsets[i + 1] as usize;
    let hub_dissuasion = params.attraction / (1.0 + f64::from(degrees[i]));
    for row in start..end {
        let neighbour = adj_indices[row] as usize;
        let dx = positions[neighbour][0] - x;
        let dy = positions[neighbour][1] - y;
        // Magnitude = hub_dissuasion · d, direction = toward the neighbour.
        // Coincident neighbours (d = 0) exert no attraction until the
        // repulsion nudge separates them.
        out[0] += dx * hub_dissuasion;
        out[1] += dy * hub_dissuasion;
    }

    // 3. Gravity — constant magnitude toward the centroid, so the map stays
    //    confined however far a body is pushed.
    let gx = centroid[0] - x;
    let gy = centroid[1] - y;
    let distance = (gx * gx + gy * gy).sqrt();
    if distance > 0.0 {
        let scale = params.gravity * masses[i] / distance;
        out[0] += gx * scale;
        out[1] += gy * scale;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta};
    use crate::model::GraphModel;

    /// A deterministic pool — the tests must not depend on the host's core
    /// count.
    fn pool(threads: usize) -> ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("a small rayon pool builds")
    }

    fn note(id: u64) -> NodeId {
        NodeId::note(id)
    }

    /// A path graph `0 — 1 — … — n-1`.
    fn path(n: u64) -> GraphSnapshot {
        let mut model = GraphModel::new();
        model.apply(
            &(0..n)
                .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
                .collect::<Vec<_>>(),
        );
        model.apply(
            &(0..n.saturating_sub(1))
                .map(|i| {
                    Delta::Edge(EdgeDelta::Added {
                        from: note(i),
                        to: note(i + 1),
                        kind: EdgeKind::Link,
                    })
                })
                .collect::<Vec<_>>(),
        );
        model.snapshot()
    }

    /// A star: node 0 at the centre, `leaves` spokes.
    fn star(leaves: u64) -> GraphSnapshot {
        let mut model = GraphModel::new();
        model.apply(
            &(0..=leaves)
                .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
                .collect::<Vec<_>>(),
        );
        model.apply(
            &(1..=leaves)
                .map(|i| {
                    Delta::Edge(EdgeDelta::Added {
                        from: note(0),
                        to: note(i),
                        kind: EdgeKind::Link,
                    })
                })
                .collect::<Vec<_>>(),
        );
        model.snapshot()
    }

    fn all_finite(layout: &Layout) -> bool {
        layout
            .positions()
            .iter()
            .all(|[x, y]| x.is_finite() && y.is_finite())
    }

    fn extent(layout: &Layout) -> f64 {
        layout
            .positions()
            .iter()
            .map(|[x, y]| x.hypot(*y))
            .fold(0.0, f64::max)
    }

    /// Run `steps` steps and return the last statistics.
    fn run(layout: &mut Layout, pool: &ThreadPool, steps: usize) -> StepStats {
        let mut stats = StepStats::default();
        for _ in 0..steps {
            stats = layout.step(pool).stats;
        }
        stats
    }

    // --- force model ------------------------------------------------------

    #[test]
    fn two_connected_nodes_settle_at_their_equilibrium_spacing() {
        // Two degree-1 bodies: repulsion k·m²/d² against attraction
        // 2·a·d/(1 + deg) — the equilibrium distance is where they balance.
        let snapshot = path(2);
        let params = LayoutParams::default();
        let mut layout = Layout::from_snapshot(&snapshot, params);
        // Start them apart so the spring has to do work.
        layout.positions[0] = [-50.0, 0.0];
        layout.positions[1] = [50.0, 0.0];
        let pool = pool(2);
        let stats = run(&mut layout, &pool, 2_000);

        assert!(all_finite(&layout));
        let separation = (layout.positions[0][0] - layout.positions[1][0]).abs();
        // k_repulsion = 2, m = 2 each → k·m² = 8; attraction per endpoint
        // a·d/(1 + deg) = d/2, and both endpoints pull, so the relative
        // closing force is d. Equilibrium: 8/d² = d → d = ∛8 = 2.
        assert!(
            (separation - 2.0_f64.cbrt()).abs() < 0.05,
            "separation {separation} should sit at the force equilibrium (~2.0)"
        );
        assert!(stats.frozen, "a settled pair must freeze: {stats:?}");
        assert!(stats.mean_displacement < params.convergence_epsilon);
        // The pair stays centred: gravity pulls the centroid to the origin.
        let centre = (layout.positions[0][0] + layout.positions[1][0]) * 0.5;
        assert!(centre.abs() < 0.5, "centroid pulled to origin, got {centre}");
    }

    #[test]
    fn repulsion_pushes_unconnected_nodes_apart() {
        // No edges → no attraction; two bodies must end up on opposite sides
        // of the origin and far apart, held only by gravity's confinement.
        let mut model = GraphModel::new();
        model.apply(&[
            Delta::Node(NodeDelta::Added { id: note(0) }),
            Delta::Node(NodeDelta::Added { id: note(1) }),
        ]);
        let snapshot = model.snapshot();
        let params = LayoutParams {
            gravity: 0.1,
            ..LayoutParams::default()
        };
        let mut layout = Layout::from_snapshot(&snapshot, params);
        let pool = pool(2);
        run(&mut layout, &pool, 400);
        let dx = layout.positions[1][0] - layout.positions[0][0];
        assert!(dx.abs() > 1.0, "bodies must separate, dx = {dx}");
        assert!(all_finite(&layout));
        // Gravity still confines them.
        assert!(extent(&layout) < 50.0, "extent {}", extent(&layout));
    }

    #[test]
    fn attraction_pulls_a_disconnected_component_together() {
        // Two isolated nodes have no edge between them, so nothing joins
        // them; a path of three does, and the ends must come closer than a
        // bare pair of far-apart nodes would.
        let mut layout = Layout::from_snapshot(&path(3), LayoutParams::default());
        layout.positions = vec![[-40.0, 0.0], [0.0, 0.0], [40.0, 0.0]];
        let pool = pool(2);
        run(&mut layout, &pool, 1_500);
        let span = (layout.positions[2][0] - layout.positions[0][0]).abs();
        assert!(span < 40.0, "the ends must be pulled in, span = {span}");
        assert!(all_finite(&layout));
    }

    #[test]
    fn hubs_repel_harder_and_are_dissuaded_from_their_leaves() {
        // On a star, the hub is the heaviest body: it must sit at the centre
        // (gravity) while the leaves spread around it (degree-weighted
        // repulsion), and no leaf may collapse onto the hub.
        let mut layout = Layout::from_snapshot(&star(24), LayoutParams::default());
        let pool = pool(4);
        run(&mut layout, &pool, 2_000);
        assert!(all_finite(&layout));

        let hub = layout.positions[0];
        let mut radii: Vec<f64> = layout.positions[1..]
            .iter()
            .map(|[x, y]| (x - hub[0]).hypot(y - hub[1]))
            .collect();
        radii.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(
            radii[0] > 0.1,
            "no leaf may collapse onto the hub, closest = {}",
            radii[0]
        );
        assert!(
            radii[23] < 10.0 * radii[0].max(0.5),
            "leaves must form a ring, radii {} … {}",
            radii[0],
            radii[23]
        );
        // The hub is the most central node.
        let centroid = [
            layout.positions.iter().map(|p| p[0]).sum::<f64>() / 25.0,
            layout.positions.iter().map(|p| p[1]).sum::<f64>() / 25.0,
        ];
        let hub_to_centre = (hub[0] - centroid[0]).hypot(hub[1] - centroid[1]);
        assert!(
            hub_to_centre < extent(&layout) * 0.1,
            "hub at {hub_to_centre} from the centroid"
        );
    }

    #[test]
    fn gravity_confines_a_larger_star() {
        let mut layout = Layout::from_snapshot(&star(200), LayoutParams::default());
        let pool = pool(4);
        let stats = run(&mut layout, &pool, 1_000);
        assert!(all_finite(&layout));
        assert_eq!(stats.diverged, 0, "no divergence on a healthy run");
        // 200 leaves at a spacing of a few units stay within a few hundred
        // units of the origin — nothing flies off.
        assert!(extent(&layout) < 1_000.0, "extent {}", extent(&layout));
    }

    // --- determinism ------------------------------------------------------

    #[test]
    fn a_step_is_bit_identical_across_pool_sizes() {
        // The force pass is parallel but has no cross-node reduction, so the
        // thread count cannot change the result. This is the determinism
        // property the worker's rayon pool relies on.
        let snapshot = star(64);
        let one = pool(1);
        let many = pool(8);

        let mut a = Layout::from_snapshot(&snapshot, LayoutParams::default());
        let mut b = Layout::from_snapshot(&snapshot, LayoutParams::default());
        for _ in 0..25 {
            a.step(&one);
            b.step(&many);
        }
        assert_eq!(a.positions(), b.positions(), "positions depend on the pool");
        assert_eq!(a.iteration(), b.iteration());
        // And the run is reproducible from the same snapshot.
        let mut c = Layout::from_snapshot(&snapshot, LayoutParams::default());
        for _ in 0..25 {
            c.step(&one);
        }
        assert_eq!(a.positions(), c.positions());
    }

    #[test]
    fn the_same_snapshot_lays_out_identically_from_scratch() {
        let snapshot = path(50);
        let a = Layout::from_snapshot(&snapshot, LayoutParams::default());
        let b = Layout::from_snapshot(&snapshot, LayoutParams::default());
        assert_eq!(a.positions(), b.positions());
        assert_eq!(a.node_ids(), b.node_ids());
    }

    #[test]
    fn cold_start_spreads_the_nodes_out() {
        // A cold start must not begin from a single point: the first step
        // would then be nothing but the co-location nudge.
        let layout = Layout::from_snapshot(&path(100), LayoutParams::default());
        let mut distances: Vec<f64> =
            layout.positions().iter().map(|[x, y]| x.hypot(*y)).collect();
        distances.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(distances[0], 0.0, "node 0 starts at the origin");
        assert!(
            distances[99] > 50.0,
            "the disc must span the map, outermost = {}",
            distances[99]
        );
    }

    // --- topology change --------------------------------------------------

    #[test]
    fn retarget_keeps_survivors_and_warms_the_newcomers() {
        let mut model = GraphModel::new();
        model.apply(&[
            Delta::Node(NodeDelta::Added { id: note(0) }),
            Delta::Node(NodeDelta::Added { id: note(1) }),
            Delta::Node(NodeDelta::Added { id: note(2) }),
        ]);
        model.apply(&[
            Delta::Edge(EdgeDelta::Added {
                from: note(0),
                to: note(1),
                kind: EdgeKind::Link,
            }),
            Delta::Edge(EdgeDelta::Added {
                from: note(1),
                to: note(2),
                kind: EdgeKind::Link,
            }),
        ]);
        let pool = pool(2);
        let mut layout = Layout::from_snapshot(&model.snapshot(), LayoutParams::default());
        run(&mut layout, &pool, 500);
        let before: Vec<[f64; 2]> = layout.positions().to_vec();

        // Add node 3 attached to node 2.
        model.apply(&[
            Delta::Node(NodeDelta::Added { id: note(3) }),
            Delta::Edge(EdgeDelta::Added {
                from: note(2),
                to: note(3),
                kind: EdgeKind::Link,
            }),
        ]);
        let stats = layout.retarget(&model.snapshot());
        assert_eq!(stats, RetargetStats { kept: 3, placed: 1, removed: 0 });
        assert!(stats.changed());
        // Survivors keep their exact positions — a topology change is not a
        // restart.
        assert_eq!(&layout.positions()[..3], &before[..]);
        // The newcomer starts near its neighbour, not at the origin and not
        // in the cold-start disc.
        let neighbour = before[2];
        let newcomer = layout.positions()[3];
        let distance = (newcomer[0] - neighbour[0]).hypot(newcomer[1] - neighbour[1]);
        assert!(
            distance < 10.0,
            "warm start puts the newcomer by its neighbour, got {distance}"
        );
        assert!(all_finite(&layout));
    }

    #[test]
    fn retarget_reports_removals_and_keeps_the_rest() {
        let mut model = GraphModel::new();
        model.apply(
            &(0..6)
                .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
                .collect::<Vec<_>>(),
        );
        let pool = pool(2);
        let mut layout = Layout::from_snapshot(&model.snapshot(), LayoutParams::default());
        run(&mut layout, &pool, 100);
        let survivor = layout.positions()[0];

        model.apply(&[
            Delta::Node(NodeDelta::Removed { id: note(1) }),
            Delta::Node(NodeDelta::Removed { id: note(4) }),
        ]);
        let snapshot = model.snapshot();
        assert_eq!(snapshot.node_count(), 4);
        let stats = layout.retarget(&snapshot);
        assert_eq!(stats.removed, 2);
        assert_eq!(stats.kept, 4);
        assert_eq!(layout.node_count(), 4);
        // The dense order shifts, but node 0 is still node 0.
        assert_eq!(layout.node_ids()[0], note(0));
        assert_eq!(layout.positions()[0], survivor);
        assert!(all_finite(&layout));
    }

    #[test]
    fn a_warm_start_lets_the_layout_move_again() {
        let snapshot = star(16);
        let params = LayoutParams::default();
        let pool = pool(2);
        let mut layout = Layout::from_snapshot(&snapshot, params);
        let frozen = run(&mut layout, &pool, 4_000);
        assert!(frozen.frozen, "a star settles: {frozen:?}");
        let settled = layout.positions().to_vec();

        // A topology change wakes it: the next steps must move the map.
        layout.retarget(&snapshot);
        assert!(!layout.is_converged());
        let moved = run(&mut layout, &pool, 3);
        assert!(
            moved.max_displacement > params.convergence_epsilon,
            "a woken layout moves: {moved:?}"
        );
        assert_ne!(layout.positions(), settled);
    }

    // --- termination and numerical hygiene --------------------------------

    #[test]
    fn a_layout_terminates_and_stays_bounded_on_a_hub_heavy_graph() {
        // Preferential-attachment-like: a few hubs, many leaves.
        let mut model = GraphModel::new();
        let nodes = 300u64;
        model.apply(
            (0..nodes)
                .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
                .collect::<Vec<Delta>>()
                .as_slice(),
        );
        let mut edges: Vec<Delta> = Vec::new();
        for i in 1..nodes {
            // Attach to a hub with probability, else to the previous node.
            let target = if i % 7 == 0 { i / 7 } else { i - 1 };
            edges.push(Delta::Edge(EdgeDelta::Added {
                from: note(i),
                to: note(target),
                kind: EdgeKind::Link,
            }));
        }
        model.apply(&edges);
        let pool = pool(4);
        let mut layout = Layout::from_snapshot(&model.snapshot(), LayoutParams::default());

        let mut converged_at = None;
        for step in 0..20_000 {
            let stats = layout.step(&pool).stats;
            assert!(all_finite(&layout), "positions must stay finite at {step}");
            assert!(
                extent(&layout) < POSITION_LIMIT,
                "positions must stay bounded at {step}"
            );
            if stats.frozen {
                converged_at = Some(step);
                break;
            }
        }
        let converged_at = converged_at.expect("the layout must freeze within 20k steps");
        assert!(
            converged_at < 20_000,
            "freezes after {converged_at} steps on a 300-node graph"
        );
    }

    #[test]
    fn co_located_nodes_are_separated_without_going_non_finite() {
        // Every node at the origin: the repulsion direction is undefined for
        // every pair, and a naive 1/d² yields NaN and the layout dies.
        let snapshot = star(32);
        let params = LayoutParams::default();
        let pool = pool(4);
        let mut layout = Layout::from_snapshot(&snapshot, params);
        for position in &mut layout.positions {
            *position = [0.0; 2];
        }
        for step in 0..200 {
            let stats = layout.step(&pool).stats;
            assert!(all_finite(&layout), "finite at step {step}: {stats:?}");
            assert_eq!(stats.diverged, 0, "no limit projection at step {step}");
        }
        let radii: Vec<f64> = layout.positions().iter().map(|[x, y]| x.hypot(*y)).collect();
        assert!(
            radii.iter().any(|r| *r > 0.5),
            "co-located bodies must separate, radii {radii:?}"
        );
    }

    #[test]
    fn a_node_at_the_centroid_of_a_cluster_is_still_repusled() {
        // A body exactly at another body's position must not receive an
        // infinite force, and must end up somewhere else.
        let mut model = GraphModel::new();
        model.apply(&[
            Delta::Node(NodeDelta::Added { id: note(0) }),
            Delta::Node(NodeDelta::Added { id: note(1) }),
        ]);
        let mut layout = Layout::from_snapshot(&model.snapshot(), LayoutParams::default());
        layout.positions = vec![[3.0, -7.0], [3.0, -7.0]];
        let pool = pool(1);
        for _ in 0..50 {
            layout.step(&pool);
        }
        assert!(all_finite(&layout));
        let distance = (layout.positions[0][0] - layout.positions[1][0])
            .hypot(layout.positions[0][1] - layout.positions[1][1]);
        assert!(distance > 0.0, "the twins must separate, distance {distance}");
    }

    #[test]
    fn an_empty_layout_steps_and_freezes() {
        let pool = pool(1);
        let mut layout = Layout::from_snapshot(&GraphModel::new().snapshot(), LayoutParams::default());
        assert_eq!(layout.node_count(), 0);
        let step = layout.step(&pool);
        assert!(step.displacements.is_empty());
        assert_eq!(step.stats.iteration, 0);
        assert_eq!(step.stats.max_displacement, 0.0);
        // An empty graph is stationary by definition.
        assert!(step.stats.frozen);
        assert!(layout.is_converged());
        layout.write_positions_f32(&mut Vec::new());
    }

    #[test]
    fn step_into_reuses_the_callers_buffer() {
        let pool = pool(1);
        let mut layout = Layout::from_snapshot(&star(8), LayoutParams::default());
        let mut buffer = Vec::new();
        let first = layout.step_into(&pool, &mut buffer);
        assert_eq!(buffer.len(), layout.node_count());
        let first_len = buffer.len();
        let second = layout.step_into(&pool, &mut buffer);
        assert_eq!(buffer.len(), first_len, "the buffer is resized, not grown");
        assert!(second.iteration > first.iteration);
    }

    #[test]
    fn write_positions_f32_is_dense_ordered_and_bounded() {
        let mut layout = Layout::from_snapshot(&star(12), LayoutParams::default());
        run(&mut layout, &pool(2), 50);
        let mut out = Vec::new();
        layout.write_positions_f32(&mut out);
        assert_eq!(out.len(), layout.node_count());
        for (index, [x, y]) in out.iter().enumerate() {
            assert!(x.is_finite() && y.is_finite(), "node {index} at {x},{y}");
            assert!(x.abs() <= POSITION_LIMIT as f32 && y.abs() <= POSITION_LIMIT as f32);
        }
        // f32 hand-off loses precision but not order of magnitude.
        for (index, [x, y]) in out.iter().enumerate() {
            let [dx, dy] = layout.positions()[index];
            assert!((f64::from(*x) - dx).abs() <= (dx.abs() * 1.0e-6 + 1.0e-3));
            assert!((f64::from(*y) - dy).abs() <= (dy.abs() * 1.0e-6 + 1.0e-3));
        }
    }

    // --- parameters -------------------------------------------------------

    #[test]
    fn sanitized_parameters_reject_junk_and_keep_sense() {
        let sane = LayoutParams::default();
        assert_eq!(sane.sanitized(), sane);

        let junk = LayoutParams {
            theta: f64::NAN,
            repulsion: -1.0,
            attraction: 0.0,
            gravity: f64::INFINITY,
            jitter_tolerance: -0.5,
            initial_speed: f64::NAN,
            speed_cap: -10.0,
            min_speed_cap: f64::NAN,
            cooling_rate: -0.1,
            speed_efficiency_min: 10.0,
            speed_efficiency_max: 0.0,
            convergence_epsilon: -1.0,
            max_step_fraction: -1.0,
            convergence_steps: 0,
            jitter: f64::NAN,
            seed: 7,
            max_tree_depth: 0,
        }
        .sanitized();
        assert_eq!(junk.theta, sane.theta);
        assert_eq!(junk.repulsion, sane.repulsion);
        assert_eq!(junk.attraction, sane.attraction);
        assert_eq!(junk.gravity, sane.gravity);
        assert_eq!(junk.jitter_tolerance, sane.jitter_tolerance);
        assert_eq!(junk.initial_speed, sane.initial_speed);
        assert_eq!(junk.speed_cap, sane.speed_cap);
        assert_eq!(junk.min_speed_cap, sane.min_speed_cap);
        assert_eq!(junk.cooling_rate, sane.cooling_rate);
        assert_eq!(junk.speed_efficiency_min, sane.speed_efficiency_min);
        assert_eq!(junk.speed_efficiency_max, sane.speed_efficiency_max);
        assert_eq!(junk.convergence_epsilon, sane.convergence_epsilon);
        assert_eq!(junk.convergence_steps, sane.convergence_steps);
        assert_eq!(junk.jitter, sane.jitter);
        assert_eq!(junk.seed, 7, "the seed is a choice, not junk");
        assert_eq!(junk.max_tree_depth, sane.max_tree_depth);

        // θ is clamped into a range the tree is defined on.
        assert_eq!(LayoutParams { theta: 4.0, ..sane }.sanitized().theta, 1.5);
        assert_eq!(LayoutParams { theta: 0.0, ..sane }.sanitized().theta, 0.05);
    }

    #[test]
    fn junk_parameters_cannot_produce_a_non_finite_layout() {
        // Even with every field at its extreme, the defensive clamps hold.
        let params = LayoutParams {
            theta: 0.05,
            repulsion: 1.0e12,
            attraction: 1.0e-12,
            gravity: 1.0e-12,
            jitter_tolerance: 100.0,
            initial_speed: 1.0e6,
            speed_cap: 1.0e6,
            min_speed_cap: 1.0e6,
            cooling_rate: 1.0e-12,
            convergence_epsilon: 1.0e-12,
            convergence_steps: 1,
            ..LayoutParams::default()
        }
        .sanitized();
        let pool = pool(4);
        let mut layout = Layout::from_snapshot(&star(64), params);
        for step in 0..300 {
            let stats = layout.step(&pool).stats;
            assert!(all_finite(&layout), "finite at step {step}");
            assert!(
                extent(&layout) <= POSITION_LIMIT,
                "bounded at step {step}: {}",
                extent(&layout)
            );
            let _ = stats;
        }
    }

    #[test]
    fn a_deep_tree_handles_hundreds_of_identical_bodies() {
        // 500 bodies on one point: the depth cap must terminate the build,
        // and the bucket force must stay finite.
        let snapshot = star(499);
        let pool = pool(4);
        let params = LayoutParams {
            max_tree_depth: 16,
            ..LayoutParams::default()
        };
        let mut layout = Layout::from_snapshot(&snapshot, params);
        for position in &mut layout.positions {
            *position = [1.0, 1.0];
        }
        for step in 0..20 {
            let stats = layout.step(&pool).stats;
            assert!(all_finite(&layout), "finite at step {step}: {stats:?}");
        }
    }

    // --- the quadtree itself ----------------------------------------------

    #[test]
    fn the_quadtree_covers_every_body() {
        let snapshot = star(50);
        let layout = Layout::from_snapshot(&snapshot, LayoutParams::default());
        let tree = QuadTree::build(
            layout.positions(),
            &layout.masses,
            LayoutParams::default().max_tree_depth,
        );
        let root = tree.regions[0];
        assert_eq!(root.bodies as usize, layout.node_count());
        assert_eq!(root.mass, layout.masses.iter().sum::<f64>());
        assert!(!root.is_single_body(), "the root of 51 bodies is a region");
        // Every body is reachable exactly once.
        let mut seen = Vec::new();
        fn walk(tree: &QuadTree, region: u32, seen: &mut Vec<u32>) {
            let r = &tree.regions[region as usize];
            if r.is_single_body() {
                seen.push(r.body);
                return;
            }
            if r.is_subdivided() {
                for child in r.children {
                    if child != NO_CHILD {
                        walk(tree, child, seen);
                    }
                }
                return;
            }
            // A bucket reports nothing individually; count it.
        }
        walk(&tree, 0, &mut seen);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), layout.node_count(), "every body is in the tree");
        // The centre of mass is the mass-weighted mean of the positions.
        let (com_x, com_y) = root.centre_of_mass();
        let mean_x: f64 = layout
            .positions()
            .iter()
            .zip(&layout.masses)
            .map(|([x, _], m)| x * m)
            .sum::<f64>()
            / root.mass;
        let mean_y: f64 = layout
            .positions()
            .iter()
            .zip(&layout.masses)
            .map(|([_, y], m)| y * m)
            .sum::<f64>()
            / root.mass;
        assert!((com_x - mean_x).abs() < 1.0e-9);
        assert!((com_y - mean_y).abs() < 1.0e-9);
    }

    #[test]
    fn a_zero_theta_repulsion_matches_the_exact_pair_sum() {
        // θ → 0 never approximates, so the tree's answer must be the exact
        // n² sum. That is the oracle for the approximation itself.
        let layout = Layout::from_snapshot(&star(12), LayoutParams::default());
        let masses = &layout.masses;
        let positions = layout.positions();
        let tree = QuadTree::build(positions, masses, MAX_TREE_DEPTH);

        for i in 0..positions.len() {
            let mut via_tree = [0.0_f64; 2];
            tree.repulsion_into(positions, masses, i, 0.0, 1.0, &mut via_tree);

            let mut exact = [0.0_f64; 2];
            for j in 0..positions.len() {
                if j == i {
                    continue;
                }
                // Away from `j`, matching `visit_repulsion`'s convention.
                let dx = positions[i][0] - positions[j][0];
                let dy = positions[i][1] - positions[j][1];
                let d = dx.hypot(dy);
                if d < MIN_SEPARATION {
                    continue;
                }
                let scale = masses[i] * masses[j] / (d * d * d);
                exact[0] += dx * scale;
                exact[1] += dy * scale;
            }
            for axis in 0..2 {
                let tolerance = 1.0e-9_f64.max(exact[axis].abs() * 1.0e-9);
                assert!(
                    (via_tree[axis] - exact[axis]).abs() < tolerance,
                    "node {i} axis {axis}: tree {} vs exact {}",
                    via_tree[axis],
                    exact[axis]
                );
            }
        }
    }

    #[test]
    fn hashes_are_uniform_enough_and_deterministic() {
        for (a, b) in [(0_u64, 0_u64), (1, 2), (u64::MAX, 7), (123_456, 654_321)] {
            assert_eq!(hash_unit(0x5EED, a, b), hash_unit(0x5EED, a, b));
            let value = hash_unit(0x5EED, a, b);
            assert!((0.0..1.0).contains(&value), "hash_unit({a}, {b}) = {value}");
        }
        assert_ne!(hash_unit(1, 2, 3), hash_unit(1, 3, 2));
        assert_ne!(hash_unit(1, 2, 3), hash_unit(2, 2, 3));

        // The nudge direction is a unit vector.
        let [x, y] = nudge(1.0, 3, 7);
        assert!(((x * x + y * y).sqrt() - 1.0).abs() < 1.0e-9);
        // …and the same one every time.
        assert_eq!(nudge(1.0, 3, 7), nudge(1.0, 3, 7));
        // The magnitude is the pair force at the separation floor, capped.
        let [x, y] = nudge(1.0e30, 3, 7);
        assert!((x * x + y * y).sqrt() <= 1.0e6 * (1.0 + 1.0e-9));
    }

    #[test]
    fn retarget_stats_only_report_change_when_something_changed() {
        let snapshot = star(4);
        let mut layout = Layout::from_snapshot(&snapshot, LayoutParams::default());
        let again = layout.retarget(&snapshot);
        assert_eq!(again.kept, 5);
        assert_eq!(again.placed, 0);
        assert_eq!(again.removed, 0);
        assert!(!again.changed());
    }

    #[test]
    fn layout_is_send_and_sync() {
        // The worker owns a Layout on its own thread and the UI reads the
        // published frames (D15).
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Layout>();
        assert_send_sync::<LayoutParams>();
        assert_send_sync::<StepStats>();
    }
}
