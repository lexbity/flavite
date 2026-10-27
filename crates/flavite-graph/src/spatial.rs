//! [`SpatialGrid`] — uniform-grid spatial index for graph hit-testing (D15).
//!
//! World coordinates are the layout space (the layout worker's output, B3);
//! the renderer converts screen px to world units through the camera zoom
//! (B6). The grid buckets nodes into square cells whose size adapts to the
//! zoom (`BASE_CELL_SIZE / zoom`), so a hit query scans a constant number of
//! cells in *screen* space. Correctness never depends on the cell size:
//! [`SpatialGrid::hit`] scans the exact range of cells overlapping the query
//! circle.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::delta::NodeId;

/// Cell size in world units at zoom 1.0 — the on-screen cell size stays
/// `BASE_CELL_SIZE` px at any zoom.
///
/// Below-spec constant: it tunes bucket memory vs. scan cost, not
/// correctness. Reversal: re-measure with B9's perf harness if bucket sizes
/// show up in frame profiles.
pub const BASE_CELL_SIZE: f64 = 64.0;

/// Cell size for a zoom level, or `None` for a non-positive or non-finite
/// zoom (defensive; the camera never produces these — B6).
fn cell_size_for_zoom(zoom: f64) -> Option<f64> {
    if zoom.is_finite() && zoom > 0.0 {
        Some(BASE_CELL_SIZE / zoom)
    } else {
        None
    }
}

/// The cell containing a finite world position.
fn cell_of(cell_size: f64, x: f64, y: f64) -> (i64, i64) {
    (
        (x / cell_size).floor() as i64,
        (y / cell_size).floor() as i64,
    )
}

/// Squared distance between two points.
fn dist2(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = ax - bx;
    let dy = ay - by;
    dx * dx + dy * dy
}

/// Update `best` with a candidate at squared distance `d2` (within `r2`).
/// Nearest wins; exact ties break to the smallest [`NodeId`].
fn consider(best: &mut Option<(f64, NodeId)>, d2: f64, id: NodeId, r2: f64) {
    if d2 > r2 {
        return;
    }
    let better = match best {
        None => true,
        Some((bd, bid)) => match d2.total_cmp(bd) {
            Ordering::Less => true,
            Ordering::Equal => id < *bid,
            Ordering::Greater => false,
        },
    };
    if better {
        *best = Some((d2, id));
    }
}

/// A node's grid entry: world position plus its cell (for O(1) move/remove).
#[derive(Debug, Clone, Copy)]
struct GridEntry {
    x: f64,
    y: f64,
    cell: (i64, i64),
}

/// Uniform-grid spatial index over world coordinates (D15: `SpatialGrid`
/// does hit-testing).
#[derive(Debug, Clone)]
pub struct SpatialGrid {
    /// World units per cell (adaptive to zoom).
    cell_size: f64,
    /// cell → nodes in that cell.
    cells: HashMap<(i64, i64), Vec<NodeId>>,
    /// node → position + cell.
    entries: HashMap<NodeId, GridEntry>,
}

impl Default for SpatialGrid {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl SpatialGrid {
    /// A grid at the given camera zoom (cell size = `BASE_CELL_SIZE / zoom`).
    /// A non-positive or non-finite zoom falls back to 1.0.
    #[must_use]
    pub fn new(zoom: f64) -> Self {
        Self {
            cell_size: cell_size_for_zoom(zoom).unwrap_or(BASE_CELL_SIZE),
            cells: HashMap::new(),
            entries: HashMap::new(),
        }
    }

    /// The current cell size in world units.
    #[must_use]
    pub fn cell_size(&self) -> f64 {
        self.cell_size
    }

    /// Adapt to a new camera zoom: rehashes all entries (O(n)).
    ///
    /// D15 quantizes zoom into scale steps, so callers invoke this per step,
    /// not per frame. A non-positive or non-finite zoom, or a redundant call
    /// (same cell size), is a no-op.
    pub fn set_zoom(&mut self, zoom: f64) {
        let Some(cell_size) = cell_size_for_zoom(zoom) else {
            return;
        };
        if cell_size.to_bits() == self.cell_size.to_bits() {
            return;
        }
        self.cell_size = cell_size;
        self.cells.clear();
        for (&id, entry) in &mut self.entries {
            entry.cell = cell_of(cell_size, entry.x, entry.y);
            self.cells.entry(entry.cell).or_default().push(id);
        }
    }

    /// Insert or move a node (upsert).
    ///
    /// A non-finite position is not storable: the node is removed instead (a
    /// node without a finite position cannot be hit-tested). Defensive — the
    /// layout must emit finite positions (B3's property test asserts it).
    pub fn insert(&mut self, id: NodeId, x: f64, y: f64) {
        if let Some(old) = self.entries.remove(&id) {
            self.remove_from_cell(id, old.cell);
        }
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        let cell = cell_of(self.cell_size, x, y);
        self.entries.insert(id, GridEntry { x, y, cell });
        self.cells.entry(cell).or_default().push(id);
    }

    /// Remove a node. Returns whether it was present.
    pub fn remove(&mut self, id: NodeId) -> bool {
        match self.entries.remove(&id) {
            Some(old) => {
                self.remove_from_cell(id, old.cell);
                true
            }
            None => false,
        }
    }

    /// Remove all nodes.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.entries.clear();
    }

    /// The nearest node within `radius` of `(x, y)` (world units), or `None`
    /// if the region is empty. Ties break to the smallest [`NodeId`]
    /// (deterministic).
    ///
    /// Non-finite queries return `None`. The scan is bounded: a query circle
    /// spanning more cells than there are entries falls back to a linear scan
    /// (§4.10 budgets — no unbounded resource use).
    #[must_use]
    pub fn hit(&self, x: f64, y: f64, radius: f64) -> Option<NodeId> {
        if !x.is_finite() || !y.is_finite() || !radius.is_finite() || radius < 0.0 {
            return None;
        }
        let r2 = radius * radius;
        let x0 = ((x - radius) / self.cell_size).floor() as i64;
        let x1 = ((x + radius) / self.cell_size).floor() as i64;
        let y0 = ((y - radius) / self.cell_size).floor() as i64;
        let y1 = ((y + radius) / self.cell_size).floor() as i64;
        // Bound the scan: more cells than entries → a linear scan is cheaper
        // and bounded (saturating: cell indices saturate at i64::MIN/MAX).
        let span_x = x1.saturating_sub(x0).saturating_add(1);
        let span_y = y1.saturating_sub(y0).saturating_add(1);
        if span_x.saturating_mul(span_y) > self.entries.len() as i64 {
            return self.hit_linear(x, y, r2);
        }
        let mut best: Option<(f64, NodeId)> = None;
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(ids) = self.cells.get(&(cx, cy)) {
                    for &id in ids {
                        if let Some(entry) = self.entries.get(&id) {
                            consider(&mut best, dist2(entry.x, entry.y, x, y), id, r2);
                        }
                    }
                }
            }
        }
        best.map(|(_, id)| id)
    }

    /// The world position of a node, if present.
    #[must_use]
    pub fn position(&self, id: NodeId) -> Option<(f64, f64)> {
        self.entries.get(&id).map(|e| (e.x, e.y))
    }

    /// Number of indexed nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the grid is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove `id` from its cell's bucket (no-op if the bucket is gone).
    fn remove_from_cell(&mut self, id: NodeId, cell: (i64, i64)) {
        if let Some(ids) = self.cells.get_mut(&cell) {
            ids.retain(|&other| other != id);
            if ids.is_empty() {
                self.cells.remove(&cell);
            }
        }
    }

    /// Linear-scan hit (the bounded fallback for huge query radii).
    fn hit_linear(&self, x: f64, y: f64, r2: f64) -> Option<NodeId> {
        let mut best: Option<(f64, NodeId)> = None;
        for (&id, entry) in &self.entries {
            consider(&mut best, dist2(entry.x, entry.y, x, y), id, r2);
        }
        best.map(|(_, id)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: u64) -> NodeId {
        NodeId::note(id)
    }

    #[test]
    fn hit_at_exact_position() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 100.0, 200.0);
        assert_eq!(grid.hit(100.0, 200.0, 5.0), Some(note(1)));
        assert_eq!(grid.hit(100.0, 200.0, 0.0), Some(note(1)));
    }

    #[test]
    fn hit_precision_within_1px() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 100.0, 200.0);
        assert_eq!(grid.hit(101.0, 200.0, 5.0), Some(note(1)));
        assert_eq!(grid.hit(100.0, 201.0, 5.0), Some(note(1)));
        assert_eq!(grid.hit(99.0, 199.0, 5.0), Some(note(1)));
        // Just outside the radius.
        assert_eq!(grid.hit(106.0, 200.0, 5.0), None);
        assert_eq!(grid.hit(100.0, 200.0, 0.5), Some(note(1)));
    }

    #[test]
    fn empty_regions_return_none() {
        let mut grid = SpatialGrid::new(1.0);
        assert_eq!(grid.hit(0.0, 0.0, 10.0), None, "empty grid");
        grid.insert(note(1), 0.0, 0.0);
        assert_eq!(grid.hit(500.0, 500.0, 10.0), None, "far away");
        assert_eq!(grid.hit(3.0, 4.0, 4.9), None, "distance 5 > radius");
    }

    #[test]
    fn radius_boundary_is_inclusive() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 5.0, 0.0);
        assert_eq!(grid.hit(0.0, 0.0, 5.0), Some(note(1)), "d == r hits");
        assert_eq!(grid.hit(0.0, 0.0, 4.999), None);
    }

    #[test]
    fn nearest_node_wins() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 0.0, 0.0);
        grid.insert(note(2), 3.0, 0.0);
        assert_eq!(grid.hit(1.0, 0.0, 10.0), Some(note(1)));
        assert_eq!(grid.hit(2.0, 0.0, 10.0), Some(note(2)));
    }

    #[test]
    fn ties_break_to_smallest_node_id() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(7), 1.0, 0.0);
        grid.insert(note(3), -1.0, 0.0);
        assert_eq!(
            grid.hit(0.0, 0.0, 5.0),
            Some(note(3)),
            "equidistant → smaller NodeId"
        );
    }

    #[test]
    fn remove_exposes_next_node() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 0.0, 0.0);
        grid.insert(note(2), 3.0, 0.0);
        assert!(grid.remove(note(1)));
        assert_eq!(grid.hit(0.0, 0.0, 10.0), Some(note(2)));
        assert!(grid.remove(note(2)));
        assert_eq!(grid.hit(0.0, 0.0, 10.0), None);
        assert!(!grid.remove(note(2)), "removing an absent node");
    }

    #[test]
    fn insert_is_an_upsert() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 0.0, 0.0);
        grid.insert(note(1), 100.0, 100.0);
        assert_eq!(grid.len(), 1);
        assert_eq!(grid.position(note(1)), Some((100.0, 100.0)));
        assert_eq!(grid.hit(0.0, 0.0, 5.0), None, "old position is vacated");
        assert_eq!(grid.hit(100.0, 100.0, 5.0), Some(note(1)));
    }

    #[test]
    fn clear_empties_the_grid() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 0.0, 0.0);
        grid.insert(note(2), 10.0, 10.0);
        grid.clear();
        assert!(grid.is_empty());
        assert_eq!(grid.len(), 0);
        assert_eq!(grid.hit(0.0, 0.0, 100.0), None);
    }

    #[test]
    fn non_finite_positions_are_not_stored() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), f64::NAN, 0.0);
        grid.insert(note(2), 0.0, f64::INFINITY);
        assert!(grid.is_empty());
        assert_eq!(grid.hit(0.0, 0.0, 1000.0), None);
        // A non-finite re-insert removes the previous entry.
        grid.insert(note(3), 1.0, 1.0);
        grid.insert(note(3), f64::NAN, f64::NAN);
        assert_eq!(grid.position(note(3)), None);
        assert!(grid.is_empty());
    }

    #[test]
    fn non_finite_queries_return_none() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 0.0, 0.0);
        assert_eq!(grid.hit(f64::NAN, 0.0, 5.0), None);
        assert_eq!(grid.hit(0.0, f64::NAN, 5.0), None);
        assert_eq!(grid.hit(0.0, 0.0, f64::NAN), None);
        assert_eq!(grid.hit(0.0, 0.0, -1.0), None);
    }

    #[test]
    fn cell_size_adapts_to_zoom() {
        let mut grid = SpatialGrid::new(1.0);
        assert_eq!(grid.cell_size(), BASE_CELL_SIZE);
        grid.set_zoom(2.0);
        assert_eq!(grid.cell_size(), BASE_CELL_SIZE / 2.0);
        grid.set_zoom(0.5);
        assert_eq!(grid.cell_size(), BASE_CELL_SIZE * 2.0);
        // Non-positive / non-finite zoom is ignored.
        grid.set_zoom(0.0);
        assert_eq!(grid.cell_size(), BASE_CELL_SIZE * 2.0);
        grid.set_zoom(f64::NAN);
        assert_eq!(grid.cell_size(), BASE_CELL_SIZE * 2.0);
        // A redundant call is a no-op.
        grid.set_zoom(0.5);
        assert_eq!(grid.cell_size(), BASE_CELL_SIZE * 2.0);
    }

    #[test]
    fn set_zoom_preserves_hits() {
        let mut grid = SpatialGrid::new(1.0);
        grid.insert(note(1), 10.0, 20.0);
        grid.insert(note(2), -300.0, 400.0);
        for zoom in [0.25, 0.5, 1.0, 2.0, 4.0, 16.0] {
            grid.set_zoom(zoom);
            assert_eq!(grid.hit(10.0, 20.0, 2.0), Some(note(1)), "zoom {zoom}");
            assert_eq!(grid.hit(-300.0, 400.0, 2.0), Some(note(2)), "zoom {zoom}");
            assert_eq!(grid.hit(0.0, 0.0, 1.0), None, "zoom {zoom}");
        }
        assert_eq!(grid.len(), 2);
    }

    #[test]
    fn hit_scans_across_cells() {
        let mut grid = SpatialGrid::new(1.0); // cell = 64 world units
        grid.insert(note(1), 0.0, 0.0);
        grid.insert(note(2), 1000.0, 0.0);
        grid.insert(note(3), 0.0, 1000.0);
        // A wide query spans many cells; nearest within radius wins.
        assert_eq!(
            grid.hit(500.0, 0.0, 600.0),
            Some(note(1)),
            "tie 1 vs 2 → smaller id"
        );
        assert_eq!(grid.hit(900.0, 0.0, 200.0), Some(note(2)));
        assert_eq!(grid.hit(500.0, 500.0, 100.0), None, "all too far");
    }

    #[test]
    fn huge_radius_uses_the_bounded_linear_fallback() {
        let mut grid = SpatialGrid::new(1.0);
        for i in 0..100u64 {
            grid.insert(note(i), (i as f64) * 3.0, (i as f64) * -7.0);
        }
        // A radius spanning ~10^15 cells must not scan cells.
        let hit = grid.hit(10.0, -20.0, 1e12);
        // Brute-force nearest (same tie-break as the grid).
        let expected = (0..100u64)
            .map(|i| {
                let dx = (i as f64) * 3.0 - 10.0;
                let dy = (i as f64) * -7.0 + 20.0;
                (dx * dx + dy * dy, note(i))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|(_, id)| id);
        assert_eq!(hit, expected);
        assert_eq!(hit, Some(note(3)), "sanity: node 3 is nearest");
    }
}
