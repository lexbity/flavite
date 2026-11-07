//! Ordered search-snapshot capture handshake (RFC-0006 S06, D-07).
//!
//! The read worker establishes its own SQL read transaction, reads the
//! committed generation G and acknowledges snapshot establishment while the
//! SQL owner holds its generation-order gate (the single-threaded actor
//! handler); the owner then inserts the ordered `Capture(G)` queue message
//! and releases the gate. The handshake is bounded to 50 ms and returns
//! Busy on timeout; it never includes Tantivy commit or evaluation.

use crate::actor::VaultActorMsg;
use crossbeam::channel::{Receiver, Sender};
use std::time::Instant;

/// Worker-side half of one capture handshake.
pub struct SearchCaptureHandshake {
    /// Signalled when the worker must establish its SQL snapshot.
    pub establish: Receiver<()>,
    /// The generation the worker read from its own snapshot.
    pub ack: Sender<(u64, u64)>,
    /// The capture ticket (searcher waiter) once the ordered capture was
    /// enqueued; `Err` carries the typed refusal (Busy on handshake
    /// timeout).
    pub ticket: Receiver<Result<flavite_index::search_session::CaptureTicket, String>>,
}

impl SearchCaptureHandshake {
    /// Begin a capture handshake with the vault actor.
    ///
    /// # Errors
    /// Returns an error when the actor is not reachable.
    pub fn begin(sender: &Sender<VaultActorMsg>) -> Result<Self, String> {
        let (establish_tx, establish_rx) = crossbeam::channel::bounded::<()>(1);
        let (ack_tx, ack_rx) = crossbeam::channel::bounded::<(u64, u64)>(1);
        let (reply_tx, reply_rx) = crossbeam::channel::bounded(1);
        sender
            .send(VaultActorMsg::SearchCaptureHandshake {
                establish: establish_tx,
                ack: ack_rx,
                reply: reply_tx,
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            establish: establish_rx,
            ack: ack_tx,
            ticket: reply_rx,
        })
    }

    /// Wait for the establish signal (the actor's gate is held while the
    /// worker reads its snapshot).
    ///
    /// # Errors
    /// Returns an error when the handshake deadline passes first.
    pub fn wait_establish(&self, deadline: Instant) -> Result<(), String> {
        self.establish
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "search capture handshake timed out".to_owned())
    }
}
