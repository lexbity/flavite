//! History resource closure: budgets, accounting, eviction, reservations
//! (FEUX-0002 X01 / E-12).
//!
//! Retained editing history (undo nodes with text snapshots and recorded
//! selection sets) is bounded by per-buffer budgets, enforced at the model's
//! install facades *before* any mutation, and by an app-wide ledger at the
//! Session. Every byte is accounted; shared `Arc` text is counted per
//! referer (over-count by design, E-12 — errors err toward earlier eviction,
//! never toward OOM). Eviction is deterministic: off-path branches oldest
//! first, then the oldest checkpointable prefix of the current path; never
//! the current node, never an admitted restore's target.
//!
//! The lease clock is always passed in as wall-clock milliseconds so fault
//! tests control time; production callers use `crate::undo::wall_clock_ms`.

/// Default per-buffer retained-history byte budget (E-12).
pub const DEFAULT_MAX_HISTORY_BYTES: u64 = 128 * 1024 * 1024;
/// Default per-buffer retained-history node budget (E-12).
pub const DEFAULT_MAX_HISTORY_NODES: u64 = 10_000;
/// App-wide retained-history byte budget across all open buffers (E-12).
pub const APP_WIDE_MAX_HISTORY_BYTES: u64 = 512 * 1024 * 1024;
/// Reservation lease length: an in-flight reservation releases itself after
/// this long even if the holder never completes (crash safety, E-12).
pub const RESERVATION_LEASE_MS: u64 = 30_000;
/// Fixed per-node metadata overhead constant. Measured at test time: the
/// `UndoNode` struct itself plus fixed strings/Vec headers must fit under
/// this constant (see `metadata_constant_covers_undo_node` in the buffer
/// test suite). Variable memory — snapshot text, edit replacement text and
/// recorded selection sets — is accounted separately.
pub const METADATA_BYTES_PER_NODE: u64 = 512;

/// A snapshot of the accounted history usage of one tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryAccount {
    /// Total accounted retained bytes (text + selections + metadata).
    pub bytes: u64,
    /// Number of retained nodes.
    pub nodes: u64,
}

/// A per-buffer history budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Byte ceiling for retained history.
    pub max_bytes: u64,
    /// Node-count ceiling for retained history.
    pub max_nodes: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_HISTORY_BYTES,
            max_nodes: DEFAULT_MAX_HISTORY_NODES,
        }
    }
}

/// A committed (or in-flight) history reservation. Byte reservations claim
/// `projected` bytes against the budget; restore-target protections carry a
/// node id and claim no bytes. Both expire after the lease window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    /// Monotonic issuance id, unique within one tree.
    pub id: u64,
    /// Wall-clock millisecond at which this reservation expires.
    pub lease_until_ms: u64,
    /// Bytes claimed against the budget (`0` for target protections).
    pub projected: u64,
    /// A node protected from eviction (in-flight restore target), if any.
    pub target: Option<u64>,
}

/// Typed budget refusal. Complete, before-mutation, zero-effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitExceeded {
    /// Bytes the refused operation would have needed.
    pub needed: u64,
    /// The budget ceiling that could not be satisfied.
    pub budget: u64,
}

/// Report of one eviction pass (deterministic over the stored sequence).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictionReport {
    /// Evicted node ids, in eviction order.
    pub evicted: Vec<u64>,
    /// Bytes released (the post-eviction reaccount delta).
    pub freed_bytes: u64,
}

/// Recorded selection-set bytes: each `[usize; 3]` member (`[id, anchor,
/// head]`) costs its three 8-byte fields plus 8 bytes of fixed per-selection
/// metadata (FEUX-0002 E-12).
#[must_use]
pub fn selection_bytes(selections: &[[usize; 3]]) -> u64 {
    selections.len() as u64 * (3 * 8 + 8)
}

/// Account one retained node: fixed metadata + the node's own snapshot text
/// (unique retained text allocation) + its edit replacement text + recorded
/// before/after selection sets.
#[must_use]
pub fn account_node(
    snapshot_text_len: usize,
    replacement_len: usize,
    before: &[[usize; 3]],
    after: &[[usize; 3]],
) -> u64 {
    METADATA_BYTES_PER_NODE
        + snapshot_text_len as u64
        + replacement_len as u64
        + selection_bytes(before)
        + selection_bytes(after)
}

/// Heap space for one exact movement batch, including its outer Vec slot.
#[must_use]
pub fn movement_bytes(spans: usize) -> u64 {
    std::mem::size_of::<Vec<crate::selection::SpanEdit>>() as u64
        + spans as u64 * std::mem::size_of::<crate::selection::SpanEdit>() as u64
}
