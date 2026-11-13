//! Operation tickets and terminal receipts (FEUX-0002 E-5, §5.5).
//!
//! Every mutating/restore activation issued through the editor kernel path
//! carries a Session-issued [`OperationId`]. Admission validates
//! incarnation/origin and ticket, deduplicates, and records a bounded
//! terminal receipt on completion; replays of retired sequences are refused,
//! and duplicates before/after completion return the original outcome.
//!
//! The ledger is process-local and single-writer (Session turns serialize
//! access); no locks beyond the Session's owning mutex are needed.

use std::collections::HashMap;

/// Maximum concurrent issued-but-not-terminal operations per buffer.
pub const MAX_PENDING_PER_BUFFER: usize = 32;
/// Maximum retained terminal receipts app-wide (oldest retired first).
pub const MAX_TERMINAL_RECEIPTS: usize = 256;

/// One issued operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId {
    /// The buffer this operation targets (ledger key).
    pub buffer_id: u64,
    /// Stable incarnation of the buffer identity (per-ledger).
    pub buffer_incarnation: u64,
    /// Monotonic sequence within that incarnation.
    pub sequence: u64,
    /// Unguessable, process-local ticket (hex on the wire).
    pub ticket: u128,
}

/// Admission status of an activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketStatus {
    /// The operation is issued and pending; the activation may proceed.
    Pending,
    /// No such operation was issued (or it belongs to a replaced buffer
    /// incarnation).
    Unknown,
    /// The operation's sequence has been retired (expired replay).
    Retired,
    /// The operation already completed as applied (duplicate delivery).
    AlreadyApplied,
    /// The operation was cancelled before install (duplicate delivery).
    Cancelled,
}

/// Terminal outcome recorded for a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
struct TerminalEntry {
    op: OperationId,
    outcome: Outcome,
}

/// The ledger.
#[derive(Debug, Default)]
pub struct ReceiptLedger {
    /// Buffer id -> incarnation counter (first issue creates it).
    incarnation: HashMap<u64, u64>,
    /// Buffer id -> last issued sequence.
    sequences: HashMap<u64, u64>,
    /// Buffer id -> issued, not-yet-terminal operations.
    pending: HashMap<u64, Vec<OperationId>>,
    /// Terminal receipts, bounded; oldest retired first.
    terminal: Vec<TerminalEntry>,
    /// Buffer id -> highest retired sequence.
    retired_watermark: HashMap<u64, u64>,
    /// Ticket generator state (xorshift seeded from process entropy).
    ticket_state: u128,
}

impl ReceiptLedger {
    /// Create a ledger seeded from process-local entropy.
    #[must_use]
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos() as u128);
        let pid = std::process::id() as u128;
        Self {
            ticket_state: (nanos << 32) ^ (pid << 8) ^ 0x9e37_79b9_7f4a_7c15,
            ..Self::default()
        }
    }

    /// Issue one operation for `buffer_id`. Returns `None` when the per-buffer
    /// pending cap is reached (AC-5: ≤32 pending enforced).
    pub fn issue(&mut self, buffer_id: u64) -> Option<OperationId> {
        let incarnation = *self.incarnation.entry(buffer_id).or_insert(1);
        let pending_len = self.pending.get(&buffer_id).map_or(0, Vec::len);
        if pending_len >= MAX_PENDING_PER_BUFFER {
            return None;
        }
        let sequence = {
            let next = self.sequences.entry(buffer_id).or_insert(0);
            *next += 1;
            *next
        };
        let ticket = self.next_ticket();
        let op = OperationId {
            buffer_id,
            buffer_incarnation: incarnation,
            sequence,
            ticket,
        };
        self.pending.entry(buffer_id).or_default().push(op);
        Some(op)
    }

    /// Admit one activation. Pending → proceed; terminal → duplicate;
    /// retired sequence → expired; else unknown.
    #[must_use]
    pub fn admit(&self, op: &OperationId) -> TicketStatus {
        if self
            .pending
            .get(&op.buffer_id)
            .is_some_and(|pending| pending.contains(op))
        {
            return TicketStatus::Pending;
        }
        for entry in &self.terminal {
            if entry.op == *op {
                return match entry.outcome {
                    Outcome::Applied => TicketStatus::AlreadyApplied,
                    Outcome::Cancelled => TicketStatus::Cancelled,
                };
            }
        }
        if self
            .retired_watermark
            .get(&op.buffer_id)
            .is_some_and(|watermark| op.sequence <= *watermark)
        {
            return TicketStatus::Retired;
        }
        TicketStatus::Unknown
    }

    /// Complete an operation with a terminal outcome.
    pub fn complete(&mut self, op: &OperationId, outcome: Outcome) {
        if let Some(pending) = self.pending.get_mut(&op.buffer_id) {
            pending.retain(|candidate| candidate != op);
        }
        self.terminal.push(TerminalEntry { op: *op, outcome });
        if self.terminal.len() > MAX_TERMINAL_RECEIPTS {
            let overflow = self.terminal.len() - MAX_TERMINAL_RECEIPTS;
            for entry in self.terminal.drain(..overflow) {
                let watermark = self
                    .retired_watermark
                    .entry(entry.op.buffer_id)
                    .or_insert(0);
                *watermark = (*watermark).max(entry.op.sequence);
            }
        }
        // A completion also retires the sequence (its replay is expired).
        let watermark = self.retired_watermark.entry(op.buffer_id).or_insert(0);
        *watermark = (*watermark).max(op.sequence);
    }

    /// Cancel a pending operation (host-initiated). Returns whether the
    /// operation was pending and consumed; a terminal op reports `false`.
    pub fn cancel(&mut self, op: &OperationId) -> bool {
        let removed = if let Some(pending) = self.pending.get_mut(&op.buffer_id) {
            let len = pending.len();
            pending.retain(|candidate| candidate != op);
            pending.len() != len
        } else {
            false
        };
        if removed {
            self.complete(op, Outcome::Cancelled);
        }
        removed
    }

    /// A buffer was closed (or replaced): its issued tickets are dead. Every
    /// pending operation is dropped and its sequences retired, so a late or
    /// replayed activation refuses `Unknown`/`Retired` instead of ever
    /// admitting against a buffer identity that no longer exists (E-5:
    /// admission validates the buffer incarnation).
    pub fn close_buffer(&mut self, buffer_id: u64) {
        self.pending.remove(&buffer_id);
        if let Some(last) = self.sequences.get(&buffer_id) {
            let watermark = self.retired_watermark.entry(buffer_id).or_insert(0);
            *watermark = (*watermark).max(*last);
        }
        let incarnation = self.incarnation.entry(buffer_id).or_insert(1);
        *incarnation += 1;
    }

    /// Test/diagnostic: number of pending operations for a buffer.
    #[must_use]
    pub fn pending_for(&self, buffer_id: u64) -> usize {
        self.pending.get(&buffer_id).map_or(0, Vec::len)
    }

    fn next_ticket(&mut self) -> u128 {
        let mut state = self.ticket_state;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.ticket_state = state;
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_admit_complete_lifecycle_and_dedup() {
        let mut ledger = ReceiptLedger::new();
        let op = ledger.issue(7).unwrap();
        assert_eq!(ledger.admit(&op), TicketStatus::Pending);
        ledger.complete(&op, Outcome::Applied);
        assert_eq!(ledger.admit(&op), TicketStatus::AlreadyApplied);
        // A replayed old sequence after retirement is expired.
        let op2 = ledger.issue(7).unwrap();
        assert_eq!(ledger.admit(&op2), TicketStatus::Pending);
        ledger.cancel(&op2);
        assert_eq!(ledger.admit(&op2), TicketStatus::Cancelled);
    }

    #[test]
    fn unknown_and_retired_are_distinct() {
        let mut ledger = ReceiptLedger::new();
        let op = ledger.issue(3).unwrap();
        let forged = OperationId {
            buffer_id: op.buffer_id,
            buffer_incarnation: op.buffer_incarnation,
            sequence: op.sequence + 5,
            ticket: 0xdead_beef,
        };
        assert_eq!(ledger.admit(&forged), TicketStatus::Unknown);
        ledger.complete(&op, Outcome::Applied);
        // The sequence is now retired; a different ticket on the same
        // sequence is refused as expired (the sequence is consumed).
        let replay = OperationId {
            buffer_id: op.buffer_id,
            buffer_incarnation: op.buffer_incarnation,
            sequence: op.sequence,
            ticket: 0x1234,
        };
        assert_eq!(ledger.admit(&replay), TicketStatus::Retired);
        // An older sequence is retired.
        let older = OperationId {
            buffer_id: op.buffer_id,
            buffer_incarnation: op.buffer_incarnation,
            sequence: op.sequence.saturating_sub(1),
            ticket: 0x4321,
        };
        assert_eq!(ledger.admit(&older), TicketStatus::Retired);
    }

    #[test]
    fn close_buffer_invalidates_its_tickets_and_bumps_the_incarnation() {
        let mut ledger = ReceiptLedger::new();
        let pending = ledger.issue(5).unwrap();
        assert_eq!(ledger.admit(&pending), TicketStatus::Pending);
        let incarnation_before = pending.buffer_incarnation;
        ledger.close_buffer(5);
        assert_eq!(ledger.pending_for(5), 0);
        // The old ticket can never admit again.
        assert_eq!(ledger.admit(&pending), TicketStatus::Retired);
        // Fresh issues carry the bumped incarnation.
        let fresh = ledger.issue(5).unwrap();
        assert_eq!(fresh.buffer_incarnation, incarnation_before + 1);
        assert_eq!(ledger.admit(&fresh), TicketStatus::Pending);
        // A forged ticket stamped with the OLD incarnation is refused.
        let stale_incarnation = OperationId {
            buffer_incarnation: incarnation_before,
            ..fresh
        };
        assert_ne!(ledger.admit(&stale_incarnation), TicketStatus::Pending);
    }

    #[test]
    fn pending_cap_is_enforced_per_buffer() {
        let mut ledger = ReceiptLedger::new();
        for _ in 0..MAX_PENDING_PER_BUFFER {
            assert!(ledger.issue(1).is_some());
        }
        assert!(ledger.issue(1).is_none());
        assert!(ledger.issue(2).is_some(), "other buffers unaffected");
    }

    #[test]
    fn terminal_receipts_are_bounded_and_retirement_advances() {
        let mut ledger = ReceiptLedger::new();
        let mut first = None;
        for i in 0..(MAX_TERMINAL_RECEIPTS + 10) {
            let op = ledger.issue(9).unwrap();
            if first.is_none() {
                first = Some(op);
            }
            ledger.complete(&op, Outcome::Applied);
            let _ = i;
        }
        assert!(ledger.terminal.len() <= MAX_TERMINAL_RECEIPTS);
        // The very first receipt was retired (evicted from the terminal list).
        assert_eq!(
            ledger.admit(&first.unwrap()),
            TicketStatus::Retired,
            "retired replay is refused"
        );
    }
}
