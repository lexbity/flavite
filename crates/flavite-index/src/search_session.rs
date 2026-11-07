//! Search snapshot capture protocol (RFC-0006 S06, D-07).
//!
//! A read worker establishes its own SQL read transaction, reads the
//! committed generation G and acknowledges snapshot establishment while the
//! SQL owner holds its generation-order gate; the owner then inserts the
//! ordered `Capture(G)` queue message and releases the gate. The handshake
//! is bounded to [`HANDSHAKE_TIMEOUT`] and returns Busy on timeout; it
//! never includes Tantivy commit or evaluation. The worker then waits
//! asynchronously for the G-stamped searcher while retaining its SQL
//! snapshot, within a total [`SNAPSHOT_LIFETIME`].

use crate::search::CapturedSearcher;
use crate::search_writer::CaptureOutcome;
use crossbeam::channel::Receiver;
use std::time::{Duration, Instant};
use thiserror::Error;

/// SQL-snapshot handshake budget (D-07). Timeout returns Busy; it MUST NOT
/// include Tantivy commit or evaluation.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(50);

/// Total snapshot lifetime from capture establishment (D-07).
pub const SNAPSHOT_LIFETIME: Duration = Duration::from_secs(30);

/// Why an admitted capture did not produce a coherent snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CaptureWaitError {
    #[error("search snapshot was invalidated by a reset; retry is explicit")]
    ResetRequired,
    #[error("search snapshot did not complete within its lifetime")]
    Timeout,
    #[error("search writer is no longer running")]
    Disconnected,
}

/// Worker-side capture ticket. The worker keeps its own SQL read
/// transaction (snapshot at G) open while waiting for the searcher.
#[derive(Debug)]
pub struct CaptureTicket {
    pub epoch: u64,
    pub generation: u64,
    pub outcome: Receiver<CaptureOutcome>,
}

impl CaptureTicket {
    /// Wait asynchronously for the ordered G-stamped searcher.
    ///
    /// # Errors
    /// Returns typed wait failures; a reset never skips a gap silently.
    pub fn wait(self, deadline: Instant) -> Result<CapturedSearcher, CaptureWaitError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CaptureWaitError::Timeout);
        }
        match self.outcome.recv_timeout(remaining) {
            Ok(CaptureOutcome::Searcher {
                epoch,
                generation,
                searcher,
            }) => {
                // Coherence: the searcher must be stamped at exactly the
                // generation and epoch the worker read (D-07: never a mixed
                // G/G+1 bundle).
                if epoch != self.epoch || generation != self.generation {
                    return Err(CaptureWaitError::ResetRequired);
                }
                Ok(searcher)
            }
            Ok(CaptureOutcome::ResetRequired) => Err(CaptureWaitError::ResetRequired),
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => Err(CaptureWaitError::Timeout),
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                Err(CaptureWaitError::Disconnected)
            }
        }
    }
}
