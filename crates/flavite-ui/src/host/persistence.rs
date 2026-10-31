//! Device-local workspace persistence host (RFC-0003 §22, W06).
//!
//! Owns the debounce, generation receipts, and retry/backoff for the
//! workspace layout file. Layout saves are independent of note saves: the
//! host encodes the durable projection of the committed workspace state and
//! writes it through the facade's user-state API; storage faults surface as
//! typed states with retry, never silent loss.

use std::time::{Duration, Instant};

/// Autosave debounce (NFR-09).
pub const LAYOUT_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// Maximum dirty age while storage is healthy (NFR-09).
pub const LAYOUT_MAX_DIRTY_AGE: Duration = Duration::from_secs(2);

/// Why a layout write failed (bounded, honest, retryable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceFailure {
    /// The device has no user-state storage.
    Unavailable,
    /// Another writer advanced the file; the host reloads and re-encodes.
    GenerationMismatch {
        /// The generation the host held.
        expected: u64,
        /// The generation actually stored.
        actual: u64,
    },
    /// The device-local store refused the operation.
    Failed {
        /// The typed failure text.
        message: String,
    },
}

/// What the host should persist next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveRequest {
    /// Nothing is due.
    None,
    /// Encode the current state and write it under this receipt.
    Write {
        /// The stored generation this write is based on.
        expected_generation: u64,
    },
}

/// Debounce/receipt state for the workspace layout autosave.
#[derive(Debug, Default)]
pub struct PersistenceHost {
    generation: u64,
    dirty_since: Option<Instant>,
    backoff_until: Option<Instant>,
    failures: u64,
    last_error: Option<PersistenceFailure>,
}

impl PersistenceHost {
    /// Mark the workspace dirty after a committed layout change. The clock
    /// is caller-supplied so tests and hosts share one timeline.
    pub fn mark_dirty(&mut self, now: Instant) {
        if self.dirty_since.is_none() {
            self.dirty_since = Some(now);
        }
    }

    /// The stored generation this host last observed.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Record a successful write's receipt.
    pub fn record_success(&mut self, generation: u64) {
        self.generation = generation;
        self.dirty_since = None;
        self.backoff_until = None;
        self.failures = 0;
        self.last_error = None;
    }

    /// Record a failed write; backoff doubles with consecutive failures.
    pub fn record_failure(&mut self, failure: PersistenceFailure) {
        self.failures = self.failures.saturating_add(1);
        self.last_error = Some(failure);
        let backoff = LAYOUT_SAVE_DEBOUNCE
            .checked_mul(1_u32 << self.failures.min(5))
            .unwrap_or(Duration::from_secs(30));
        self.backoff_until = Some(Instant::now() + backoff);
        // The dirty window reopens so the max-dirty-age deadline keeps the
        // write attempt alive.
        self.dirty_since = Some(Instant::now());
    }

    /// The last failure, for the restoration/diagnostics report.
    #[must_use]
    pub const fn last_error(&self) -> Option<&PersistenceFailure> {
        self.last_error.as_ref()
    }

    /// Consecutive failure count (diagnostics).
    #[must_use]
    pub const fn failures(&self) -> u64 {
        self.failures
    }

    /// Seed the receipt from a freshly read file (attach/restore path).
    pub fn observe_generation(&mut self, generation: u64) {
        self.generation = generation;
    }

    /// Whether a restore attempt is currently blocked by backoff.
    #[must_use]
    pub fn backing_off(&self, now: Instant) -> bool {
        self.backoff_until.is_some_and(|until| now < until)
    }

    /// Decide whether a save is due now (debounce elapsed or dirty too long,
    /// never during backoff).
    #[must_use]
    pub fn due(&self, now: Instant) -> SaveRequest {
        let Some(dirty_since) = self.dirty_since else {
            return SaveRequest::None;
        };
        if self.backing_off(now) {
            return SaveRequest::None;
        }
        if now.duration_since(dirty_since) >= LAYOUT_SAVE_DEBOUNCE
            || now.duration_since(dirty_since) >= LAYOUT_MAX_DIRTY_AGE
        {
            SaveRequest::Write {
                expected_generation: self.generation,
            }
        } else {
            SaveRequest::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_then_due_then_receipt() {
        let mut host = PersistenceHost::default();
        let start = Instant::now();
        assert_eq!(host.due(start), SaveRequest::None, "not dirty");
        host.mark_dirty(start);
        assert_eq!(host.due(start), SaveRequest::None, "inside debounce");
        assert_eq!(
            host.due(start + LAYOUT_SAVE_DEBOUNCE),
            SaveRequest::Write {
                expected_generation: 0
            }
        );
        host.record_success(7);
        assert_eq!(host.generation(), 7);
        assert_eq!(host.due(start + LAYOUT_SAVE_DEBOUNCE), SaveRequest::None);
    }

    #[test]
    fn max_dirty_age_overrides_backoff_and_failures_back_off() {
        let mut host = PersistenceHost::default();
        let start = Instant::now();
        host.mark_dirty(start);
        host.record_failure(PersistenceFailure::Unavailable);
        assert!(
            host.backing_off(start + LAYOUT_SAVE_DEBOUNCE),
            "immediately after a failure the host backs off"
        );
        // The max-dirty-age deadline keeps a write attempt alive.
        let long = start + LAYOUT_MAX_DIRTY_AGE * 3;
        assert!(
            matches!(host.due(long), SaveRequest::Write { .. }),
            "max dirty age forces an attempt despite backoff"
        );
        assert!(host.failures() >= 1);
    }
}
