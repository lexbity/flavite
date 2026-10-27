//! Rolling frame-time statistics for the S2 harness (p50/p95/p99).
//!
//! The probe measures two different things and reports both, because a
//! vsync-locked frame interval alone hides widget cost:
//!
//! - **Frame interval** — wall time between `iced::window::frames()` ticks.
//!   On a 60 Hz display this saturates at ≈16.67 ms, so it answers "did we
//!   miss a frame?", not "how fast could we go?".
//! - **Widget CPU time** — what `Primitive::prepare`/`Primitive::draw`
//!   actually cost, recorded through [`ProbeTimings`]. This is the number
//!   that changes when the renderer gets faster or slower.
//!
//! [`ProbeTimings`]: crate::graph::shader_probe::ProbeTimings

/// Nearest-rank percentile of `samples` at `p` (0–100).
///
/// Returns `0.0` for an empty slice and clamps `p` to `[0, 100]`. Sorting
/// happens on a copy, so the caller's order is untouched.
///
/// Nearest-rank (`ceil(p/100 · n) - 1`) is the textbook definition for
/// discrete samples: `percentile(v, 100)` is the maximum, `percentile(v, 0)`
/// the minimum, and no answer is ever interpolated between two frames that
/// did not happen.
#[must_use]
pub fn percentile(samples: &[f32], p: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    if !p.is_finite() || p <= 0.0 {
        return sorted[0];
    }
    if p >= 100.0 {
        return *sorted.last().expect("non-empty");
    }
    let rank = (f64::from(p) / 100.0 * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    sorted[index]
}

/// An immutable view of a [`FrameStats`] window, safe to print or serialize.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatsSnapshot {
    /// Samples in the window.
    pub count: usize,
    /// Smallest sample, milliseconds.
    pub min_ms: f32,
    /// 50th percentile, milliseconds.
    pub p50_ms: f32,
    /// 95th percentile, milliseconds.
    pub p95_ms: f32,
    /// 99th percentile, milliseconds.
    pub p99_ms: f32,
    /// Largest sample, milliseconds.
    pub max_ms: f32,
    /// Arithmetic mean, milliseconds.
    pub mean_ms: f32,
}

impl StatsSnapshot {
    /// Rate implied by the 50th percentile, in frames per second
    /// (`0.0` when the window is empty).
    #[must_use]
    pub fn p50_fps(&self) -> f32 {
        if self.p50_ms > 0.0 {
            1000.0 / self.p50_ms
        } else {
            0.0
        }
    }

    /// Rendered a human-readable row for the probe's report table.
    #[must_use]
    pub fn report_row(&self, label: &str) -> String {
        format!(
            "{label:<18} {:>7} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.1}",
            self.count,
            self.min_ms,
            self.p50_ms,
            self.p95_ms,
            self.p99_ms,
            self.max_ms,
            self.mean_ms,
        )
    }
}

/// The header for [`StatsSnapshot::report_row`], built with the same field
/// widths so every column starts on the same byte in both lines.
#[must_use]
pub fn report_header() -> String {
    format!(
        "{:<18} {:>7} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "metric", "samples", "min", "p50", "p95", "p99", "max", "mean"
    )
}

/// A fixed-capacity rolling window of frame samples, in milliseconds.
///
/// Oldest samples are overwritten rather than dropped or grown: the window
/// describes *recent* frames, memory stays constant for an arbitrarily long
/// run, and the probe can run for hours without a leak.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameStats {
    window: Vec<f32>,
    /// Next slot to write (ring index).
    head: usize,
    /// Slots filled so far (caps at `window.len()`).
    len: usize,
}

impl FrameStats {
    /// A rolling window of the last `capacity` samples.
    ///
    /// A capacity of 0 is rejected (rounded up to 1) — an empty window can
    /// never produce a percentile.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            window: vec![0.0; capacity.max(1)],
            head: 0,
            len: 0,
        }
    }

    /// Record one sample, in milliseconds. Non-finite values are ignored so
    /// a clock anomaly cannot poison the percentiles.
    pub fn record(&mut self, ms: f32) {
        if !ms.is_finite() {
            return;
        }
        self.window[self.head] = ms;
        self.head = (self.head + 1) % self.window.len();
        self.len = (self.len + 1).min(self.window.len());
    }

    /// Samples currently held, oldest first.
    #[must_use]
    pub fn samples(&self) -> Vec<f32> {
        if self.len < self.window.len() {
            return self.window[..self.len].to_vec();
        }
        let mut out = Vec::with_capacity(self.window.len());
        out.extend_from_slice(&self.window[self.head..]);
        out.extend_from_slice(&self.window[..self.head]);
        out
    }

    /// Number of samples held (≤ capacity).
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no samples are held yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The rolling window's capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.window.len()
    }

    /// Percentile at `p` over the held samples (`0.0` when empty).
    #[must_use]
    pub fn percentile(&self, p: f32) -> f32 {
        percentile(&self.samples(), p)
    }

    /// 50th percentile.
    #[must_use]
    pub fn p50(&self) -> f32 {
        self.percentile(50.0)
    }

    /// 95th percentile.
    #[must_use]
    pub fn p95(&self) -> f32 {
        self.percentile(95.0)
    }

    /// 99th percentile.
    #[must_use]
    pub fn p99(&self) -> f32 {
        self.percentile(99.0)
    }

    /// Summarize the window for reporting.
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        let samples = self.samples();
        let count = samples.len();
        if count == 0 {
            return StatsSnapshot {
                count: 0,
                min_ms: 0.0,
                p50_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
                max_ms: 0.0,
                mean_ms: 0.0,
            };
        }
        let sum: f32 = samples.iter().sum();
        StatsSnapshot {
            count,
            min_ms: percentile(&samples, 0.0),
            p50_ms: percentile(&samples, 50.0),
            p95_ms: percentile(&samples, 95.0),
            p99_ms: percentile(&samples, 99.0),
            max_ms: percentile(&samples, 100.0),
            mean_ms: sum / count as f32,
        }
    }

    /// Drop every sample.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        // One second of 60 fps frames — the window that a p99 over a
        // "sustained" run should be judged on.
        Self::with_capacity(60 * 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_of_known_samples() {
        // 1..=100
        let samples: Vec<f32> = (1..=100).map(|value| value as f32).collect();
        assert_eq!(percentile(&samples, 0.0), 1.0);
        assert_eq!(percentile(&samples, 50.0), 50.0);
        assert_eq!(percentile(&samples, 95.0), 95.0);
        assert_eq!(percentile(&samples, 99.0), 99.0);
        assert_eq!(percentile(&samples, 100.0), 100.0);
        // Nearest-rank never invents a value outside the sample set.
        assert!(samples.contains(&percentile(&samples, 77.0)));
    }

    #[test]
    fn percentile_edges_and_junk_inputs() {
        assert_eq!(percentile(&[], 50.0), 0.0, "empty window → 0");
        assert_eq!(percentile(&[], 0.0), 0.0);
        let one = [42.5f32];
        for p in [0.0f32, 1.0, 50.0, 99.9, 100.0, -5.0, 150.0, f32::NAN] {
            assert_eq!(percentile(&one, p), 42.5, "single sample at p={p}");
        }
        // Unsorted input is handled without mutating the caller.
        let unsorted = [3.0f32, 1.0, 2.0];
        assert_eq!(percentile(&unsorted, 50.0), 2.0);
        assert_eq!(unsorted, [3.0, 1.0, 2.0]);
        // NaN samples sort to the end instead of poisoning the order.
        let with_nan = [1.0f32, f32::NAN, 3.0];
        assert_eq!(percentile(&with_nan, 0.0), 1.0);
        assert_eq!(percentile(&with_nan, 100.0), 3.0);
    }

    #[test]
    fn ring_window_keeps_the_most_recent_samples() {
        let mut stats = FrameStats::with_capacity(3);
        assert!(stats.is_empty());
        stats.record(10.0);
        stats.record(20.0);
        stats.record(30.0);
        assert_eq!(stats.len(), 3);
        assert_eq!(stats.samples(), vec![10.0, 20.0, 30.0]);

        // `head` is the next slot to write, which is also the oldest
        // sample — so reading `head..` then `..head` is oldest-first.
        // Recording 40 overwrites 10 and leaves the window in rotation.
        stats.record(40.0);
        assert_eq!(stats.samples(), vec![20.0, 30.0, 40.0]);
        stats.record(50.0);
        assert_eq!(stats.samples(), vec![30.0, 40.0, 50.0]);
        stats.record(60.0);
        assert_eq!(stats.samples(), vec![40.0, 50.0, 60.0]);
        assert_eq!(stats.len(), 3, "length is capped at capacity");
        assert_eq!(stats.capacity(), 3);
    }

    #[test]
    fn record_ignores_non_finite_samples() {
        let mut stats = FrameStats::with_capacity(4);
        stats.record(10.0);
        stats.record(f32::NAN);
        stats.record(f32::INFINITY);
        stats.record(-1.0);
        assert_eq!(stats.len(), 2, "NaN and infinity dropped");
        assert_eq!(stats.samples(), vec![10.0, -1.0]);
        assert_eq!(stats.p50(), -1.0);
    }

    #[test]
    fn snapshot_reports_all_percentiles() {
        let mut stats = FrameStats::with_capacity(1000);
        for i in 1..=1000 {
            stats.record(i as f32);
        }
        let snap = stats.snapshot();
        assert_eq!(snap.count, 1000);
        assert_eq!(snap.min_ms, 1.0);
        assert_eq!(snap.max_ms, 1000.0);
        assert!((snap.p50_ms - 500.0).abs() < 1.0);
        assert!((snap.p95_ms - 950.0).abs() < 1.0);
        assert!((snap.p99_ms - 990.0).abs() < 1.0);
        assert!((snap.mean_ms - 500.5).abs() < 1e-3);
        assert!((snap.p50_fps() - 1000.0 / snap.p50_ms).abs() < 1e-3);
    }

    #[test]
    fn empty_snapshot_is_all_zeroes() {
        let stats = FrameStats::with_capacity(10);
        let snap = stats.snapshot();
        assert_eq!(snap.count, 0);
        assert_eq!(snap.min_ms, 0.0);
        assert_eq!(snap.p50_ms, 0.0);
        assert_eq!(snap.p95_ms, 0.0);
        assert_eq!(snap.p99_ms, 0.0);
        assert_eq!(snap.max_ms, 0.0);
        assert_eq!(snap.mean_ms, 0.0);
        assert_eq!(snap.p50_fps(), 0.0);
        assert_eq!(stats.p50(), 0.0);
        assert_eq!(stats.p95(), 0.0);
        assert_eq!(stats.p99(), 0.0);
    }

    #[test]
    fn clear_resets_the_window() {
        let mut stats = FrameStats::with_capacity(4);
        stats.record(1.0);
        stats.record(2.0);
        stats.clear();
        assert!(stats.is_empty());
        assert_eq!(stats.samples(), Vec::<f32>::new());
        stats.record(9.0);
        assert_eq!(stats.samples(), vec![9.0]);
    }

    #[test]
    fn default_window_is_one_second_at_60_fps() {
        let stats = FrameStats::default();
        assert_eq!(stats.capacity(), 3600);
        assert!(stats.is_empty());
    }

    #[test]
    fn zero_capacity_window_still_produces_a_percentile() {
        let mut stats = FrameStats::with_capacity(0);
        assert_eq!(stats.capacity(), 1, "capacity 0 is rounded up to 1");
        stats.record(5.0);
        stats.record(7.0);
        assert_eq!(stats.samples(), vec![7.0]);
        assert_eq!(stats.p99(), 7.0);
    }

    /// Byte offset at which each whitespace-separated column starts.
    fn column_starts(line: &str) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut in_whitespace = true;
        for (index, character) in line.char_indices() {
            let whitespace = character.is_whitespace();
            if in_whitespace && !whitespace {
                starts.push(index);
            }
            in_whitespace = whitespace;
        }
        starts
    }

    /// Byte offset just *past* each whitespace-separated column.
    ///
    /// See the test below for why ends, not starts.
    fn column_ends(line: &str) -> Vec<usize> {
        let mut ends = Vec::new();
        let mut in_token = false;
        for (index, character) in line.char_indices() {
            let whitespace = character.is_whitespace();
            if !whitespace && !in_token {
                in_token = true;
            } else if whitespace && in_token {
                ends.push(index);
                in_token = false;
            }
        }
        if in_token {
            ends.push(line.len());
        }
        ends
    }

    #[test]
    fn report_row_is_aligned_with_the_header() {
        let mut stats = FrameStats::with_capacity(3);
        stats.record(16.7);
        stats.record(16.8);
        stats.record(33.5);
        let row = stats.snapshot().report_row("frame");
        let header = report_header();

        assert!(row.starts_with("frame"));
        for expected in ["3", "16.700", "33.500"] {
            assert!(row.contains(expected), "row should show {expected}: {row}");
        }

        let header_columns: Vec<&str> = header.split_whitespace().collect();
        assert_eq!(
            header_columns,
            [
                "metric", "samples", "min", "p50", "p95", "p99", "max", "mean"
            ]
        );
        assert_eq!(row.split_whitespace().count(), header_columns.len());

        // Every column but the label is right-aligned, so its text *ends*
        // exactly on the field boundary even when the field is wider than the
        // text. Token starts do not have that property — a padded field opens
        // with spaces — so the ends are what must line up.
        assert_eq!(&column_ends(&row)[1..], &column_ends(&header)[1..]);
        assert_eq!(
            column_ends(&header),
            [6, 26, 35, 44, 53, 62, 71, 80],
            "the header's own field boundaries"
        );

        // The label column is left-aligned, so only its start is fixed — and
        // both lines must be the same total width.
        assert_eq!(column_starts(&row)[0], 0);
        assert_eq!(column_starts(&header)[0], 0);
        assert_eq!(row.len(), header.len());
    }
}
