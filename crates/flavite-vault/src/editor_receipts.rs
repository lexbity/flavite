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

use std::collections::{HashMap, VecDeque};

/// Maximum concurrent issued-but-not-terminal operations per buffer.
pub const MAX_PENDING_PER_BUFFER: usize = 32;
/// Maximum retained terminal receipts per buffer (oldest retired first).
pub const MAX_TERMINAL_RECEIPTS: usize = 256;
/// Per-buffer retained receipt data, including selection vectors.
pub const MAX_TERMINAL_BYTES: usize = 2 * 1024 * 1024;

/// Transient authority for a committed editor view. Headless operations have
/// no view stamp, but always bind the buffer revision when issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EditorViewStamp {
    pub view: u128,
    pub incarnation: u64,
    pub selection_version: u64,
}

/// One issued operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId {
    /// The buffer this operation targets (ledger key).
    pub buffer_id: u64,
    pub revision: u64,
    pub view: Option<EditorViewStamp>,
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

#[derive(Debug, Clone)]
struct TerminalEntry {
    op: OperationId,
    outcome: Outcome,
    receipt: Option<Box<crate::session::SessionResult>>,
    bytes: usize,
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
    terminal: HashMap<u64, VecDeque<TerminalEntry>>,
    /// Buffer id -> highest retired sequence.
    retired_watermark: HashMap<u64, u64>,
}

impl ReceiptLedger {
    /// Create an empty ledger. Tickets use operating-system randomness.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue one operation for `buffer_id`. Returns `None` when the per-buffer
    /// pending cap is reached (AC-5: ≤32 pending enforced).
    pub fn issue(
        &mut self,
        buffer_id: u64,
        revision: u64,
        view: Option<EditorViewStamp>,
    ) -> Option<OperationId> {
        let incarnation = *self.incarnation.entry(buffer_id).or_insert(1);
        if incarnation == u64::MAX {
            return None;
        }
        let pending_len = self.pending.get(&buffer_id).map_or(0, Vec::len);
        if pending_len >= MAX_PENDING_PER_BUFFER {
            return None;
        }
        let sequence = {
            let next = self.sequences.entry(buffer_id).or_insert(0);
            *next = next.checked_add(1)?;
            *next
        };
        let ticket = uuid::Uuid::new_v4().as_u128();
        let op = OperationId {
            buffer_id,
            revision,
            view,
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
        if self.incarnation.get(&op.buffer_id) != Some(&op.buffer_incarnation) {
            return TicketStatus::Unknown;
        }
        if self
            .pending
            .get(&op.buffer_id)
            .is_some_and(|pending| pending.contains(op))
        {
            return TicketStatus::Pending;
        }
        for entry in self.terminal.get(&op.buffer_id).into_iter().flatten() {
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
        // Only an issued operation may enter the terminal ledger. Completion
        // is idempotent: a late success cannot overwrite cancellation.
        if self.admit(op) != TicketStatus::Pending {
            return;
        }
        if let Some(pending) = self.pending.get_mut(&op.buffer_id) {
            pending.retain(|candidate| candidate != op);
        }
        let terminal = self.terminal.entry(op.buffer_id).or_default();
        terminal.push_back(TerminalEntry {
            op: *op,
            outcome,
            receipt: None,
            bytes: std::mem::size_of::<TerminalEntry>(),
        });
        while terminal.len() > MAX_TERMINAL_RECEIPTS {
            let entry = terminal.pop_front().expect("nonempty terminal ledger");
            let watermark = self.retired_watermark.entry(op.buffer_id).or_insert(0);
            *watermark = (*watermark).max(entry.op.sequence);
        }
    }

    /// Retain the exact original outcome; duplicate deliveries never install
    /// again and cannot borrow authority from the current view state.
    pub fn record_receipt(&mut self, op: &OperationId, result: &crate::session::SessionResult) {
        let outcome = if matches!(result, crate::session::SessionResult::Success(_)) {
            Outcome::Applied
        } else {
            Outcome::Cancelled
        };
        self.complete(op, outcome);
        let Some(terminal) = self.terminal.get_mut(&op.buffer_id) else {
            return;
        };
        if let Some(entry) = terminal
            .iter_mut()
            .find(|entry| entry.op == *op && entry.outcome == outcome && entry.receipt.is_none())
        {
            entry.bytes += receipt_bytes(result);
            entry.receipt = Some(Box::new(result.clone()));
        }
        // Count variable sets and movement vectors, not merely receipt count.
        while terminal.iter().map(|entry| entry.bytes).sum::<usize>() > MAX_TERMINAL_BYTES {
            let entry = terminal.pop_front().expect("nonempty byte-capped ledger");
            let watermark = self.retired_watermark.entry(op.buffer_id).or_insert(0);
            *watermark = (*watermark).max(entry.op.sequence);
        }
    }

    pub fn receipt(&self, op: &OperationId) -> Option<crate::session::SessionResult> {
        if self.incarnation.get(&op.buffer_id) != Some(&op.buffer_incarnation) {
            return None;
        }
        self.terminal
            .get(&op.buffer_id)?
            .iter()
            .find(|entry| entry.op == *op)?
            .receipt
            .as_deref()
            .cloned()
    }

    /// Cancel a pending operation (host-initiated). Returns whether the
    /// operation was pending and consumed; a terminal op reports `false`.
    pub fn cancel(&mut self, op: &OperationId) -> bool {
        if self.admit(op) != TicketStatus::Pending {
            return false;
        }
        self.complete(op, Outcome::Cancelled);
        true
    }

    pub fn cancel_view(&mut self, view: u128) {
        let pending: Vec<_> = self
            .pending
            .values()
            .flatten()
            .filter(|op| op.view.is_some_and(|stamp| stamp.view == view))
            .copied()
            .collect();
        for op in pending {
            self.cancel(&op);
        }
    }

    /// A buffer was closed (or replaced): its issued tickets are dead. Every
    /// pending operation is dropped and its sequences retired, so a late or
    /// replayed activation refuses `Unknown`/`Retired` instead of ever
    /// admitting against a buffer identity that no longer exists (E-5:
    /// admission validates the buffer incarnation).
    pub fn close_buffer(&mut self, buffer_id: u64) {
        self.pending.remove(&buffer_id);
        self.terminal.remove(&buffer_id);
        if let Some(last) = self.sequences.get(&buffer_id) {
            let watermark = self.retired_watermark.entry(buffer_id).or_insert(0);
            *watermark = (*watermark).max(*last);
        }
        let incarnation = self.incarnation.entry(buffer_id).or_insert(1);
        *incarnation = incarnation.saturating_add(1);
    }

    /// Test/diagnostic: number of pending operations for a buffer.
    #[must_use]
    pub fn pending_for(&self, buffer_id: u64) -> usize {
        self.pending.get(&buffer_id).map_or(0, Vec::len)
    }
}

fn receipt_bytes(result: &crate::session::SessionResult) -> usize {
    use crate::session::{CommandResult, SessionResult};
    let movement_bytes = |movement: &flavite_buffer::history::HistoryMovement| {
        movement
            .selections
            .as_ref()
            .map_or(0, |set| set.capacity() * std::mem::size_of::<[usize; 3]>())
            + movement.spans.capacity()
                * std::mem::size_of::<flavite_buffer::history::MovementSpan>()
            + movement.step_ends.capacity() * std::mem::size_of::<usize>()
    };
    std::mem::size_of::<SessionResult>()
        + match result {
            SessionResult::Success(CommandResult::EditsApplied {
                movement,
                after_selections,
                ..
            }) => {
                movement.as_ref().map_or(0, movement_bytes)
                    + after_selections
                        .as_ref()
                        .map_or(0, |set| set.capacity() * std::mem::size_of::<[usize; 3]>())
            }
            SessionResult::Success(
                CommandResult::UndoPerformed { movement, .. }
                | CommandResult::RedoPerformed { movement, .. },
            ) => movement.as_ref().map_or(0, movement_bytes),
            SessionResult::Success(CommandResult::EditingStateRestored { movement, .. }) => {
                movement_bytes(movement)
            }
            SessionResult::Error { message } => message.capacity(),
            SessionResult::ActionRequired { data, .. } => data.as_ref().map_or(0, String::capacity),
            _ => 0,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_admit_complete_lifecycle_and_dedup() {
        let mut ledger = ReceiptLedger::new();
        let op = ledger.issue(7, 0, None).unwrap();
        assert_eq!(ledger.admit(&op), TicketStatus::Pending);
        ledger.complete(&op, Outcome::Applied);
        assert_eq!(ledger.admit(&op), TicketStatus::AlreadyApplied);
        // A replayed old sequence after retirement is expired.
        let op2 = ledger.issue(7, 0, None).unwrap();
        assert_eq!(ledger.admit(&op2), TicketStatus::Pending);
        ledger.cancel(&op2);
        assert_eq!(ledger.admit(&op2), TicketStatus::Cancelled);
    }

    #[test]
    fn unknown_and_retired_are_distinct() {
        let mut ledger = ReceiptLedger::new();
        let op = ledger.issue(3, 0, None).unwrap();
        let forged = OperationId {
            buffer_id: op.buffer_id,
            revision: op.revision,
            view: op.view,
            buffer_incarnation: op.buffer_incarnation,
            sequence: op.sequence + 5,
            ticket: 0xdead_beef,
        };
        assert_eq!(ledger.admit(&forged), TicketStatus::Unknown);
        ledger.complete(&op, Outcome::Applied);
        // An incorrect ticket cannot impersonate a retained completion.
        let replay = OperationId {
            buffer_id: op.buffer_id,
            revision: op.revision,
            view: op.view,
            buffer_incarnation: op.buffer_incarnation,
            sequence: op.sequence,
            ticket: 0x1234,
        };
        assert_eq!(ledger.admit(&replay), TicketStatus::Unknown);
        // A sequence which was never issued is unknown.
        let older = OperationId {
            buffer_id: op.buffer_id,
            revision: op.revision,
            view: op.view,
            buffer_incarnation: op.buffer_incarnation,
            sequence: op.sequence.saturating_sub(1),
            ticket: 0x4321,
        };
        assert_eq!(ledger.admit(&older), TicketStatus::Unknown);
    }

    #[test]
    fn close_buffer_invalidates_its_tickets_and_bumps_the_incarnation() {
        let mut ledger = ReceiptLedger::new();
        let pending = ledger.issue(5, 0, None).unwrap();
        assert_eq!(ledger.admit(&pending), TicketStatus::Pending);
        let incarnation_before = pending.buffer_incarnation;
        ledger.close_buffer(5);
        assert_eq!(ledger.pending_for(5), 0);
        // The old ticket can never admit again.
        assert_eq!(ledger.admit(&pending), TicketStatus::Unknown);
        // Fresh issues carry the bumped incarnation.
        let fresh = ledger.issue(5, 0, None).unwrap();
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
            assert!(ledger.issue(1, 0, None).is_some());
        }
        assert!(ledger.issue(1, 0, None).is_none());
        assert!(
            ledger.issue(2, 0, None).is_some(),
            "other buffers unaffected"
        );
    }

    #[test]
    fn terminal_receipts_are_bounded_and_retirement_advances() {
        let mut ledger = ReceiptLedger::new();
        let mut first = None;
        for i in 0..(MAX_TERMINAL_RECEIPTS + 10) {
            let op = ledger.issue(9, 0, None).unwrap();
            if first.is_none() {
                first = Some(op);
            }
            ledger.complete(&op, Outcome::Applied);
            let _ = i;
        }
        assert!(ledger.terminal[&9].len() <= MAX_TERMINAL_RECEIPTS);
        // The very first receipt was retired (evicted from the terminal list).
        assert_eq!(
            ledger.admit(&first.unwrap()),
            TicketStatus::Retired,
            "retired replay is refused"
        );
    }
    #[test]
    fn completion_cannot_overwrite_cancel_or_forge_a_receipt() {
        let mut ledger = ReceiptLedger::default();
        let op = ledger.issue(1, 0, None).unwrap();
        let second = ledger.issue(1, 0, None).unwrap();
        assert_ne!(op.ticket, second.ticket);
        assert_ne!(op.ticket, 0);
        assert!(ledger.cancel(&op));
        ledger.complete(&op, Outcome::Applied);
        assert_eq!(ledger.admit(&op), TicketStatus::Cancelled);
        ledger.complete(
            &OperationId {
                ticket: 0,
                ..second
            },
            Outcome::Applied,
        );
        assert_eq!(ledger.admit(&second), TicketStatus::Pending);
        assert_eq!(ledger.terminal[&1].len(), 1);
    }

    #[test]
    fn activity_in_another_buffer_does_not_expire_a_receipt() {
        let mut ledger = ReceiptLedger::new();
        let first = ledger.issue(1, 0, None).unwrap();
        ledger.complete(&first, Outcome::Applied);
        for _ in 0..(MAX_TERMINAL_RECEIPTS * 2) {
            let other = ledger.issue(2, 0, None).unwrap();
            ledger.complete(&other, Outcome::Applied);
        }
        assert_eq!(ledger.admit(&first), TicketStatus::AlreadyApplied);
        assert_eq!(ledger.terminal[&1].len(), 1);
        assert_eq!(ledger.terminal[&2].len(), MAX_TERMINAL_RECEIPTS);
    }
    #[test]
    fn large_receipts_expire_by_bytes_and_duplicates_keep_the_original() {
        use crate::session::{CommandResult, SessionResult};
        let mut ledger = ReceiptLedger::new();
        let first = ledger.issue(9, 0, None).unwrap();
        let receipt = SessionResult::Success(CommandResult::EditsApplied {
            buffer_id: 9.into(),
            new_revision: flavite_buffer::Revision::new(1),
            movement: None,
            after_selections: Some(vec![[0, 0, 0]; 10_000]),
        });
        ledger.record_receipt(&first, &receipt);
        ledger.record_receipt(
            &first,
            &SessionResult::Error {
                message: "late wrong outcome".into(),
            },
        );
        assert_eq!(
            format!("{:?}", ledger.receipt(&first).unwrap()),
            format!("{receipt:?}")
        );
        for _ in 0..12 {
            let op = ledger.issue(9, 0, None).unwrap();
            ledger.record_receipt(&op, &receipt);
        }
        assert!(
            ledger.terminal[&9]
                .iter()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                <= MAX_TERMINAL_BYTES
        );
        assert_eq!(ledger.admit(&first), TicketStatus::Retired);
    }
}
