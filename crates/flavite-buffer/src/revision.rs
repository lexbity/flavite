//! Buffer identity, revision tracking, and edit preconditions (§4.5.2, §4.16.1).
//!
//! Every opened buffer has:
//! - An opaque instance **identity** (independent of path and cache note_id).
//! - A monotonically increasing **revision** number.
//! - A **base_hash** tracking what we last loaded or saved.
//!
//! Edits carry buffer identity + expected revision; disk writes also carry
//! `base_hash`. Stale edits return `StaleRevision` without applying any part.
//!
//! This module provides the foundation for C10's revisioned read views and
//! typed TextByte edit preconditions.

use flavite_syntax::text::TextSnapshot;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique identifier for a buffer instance.
///
/// Independent of path and cache note_id. Reopen creates a new identity;
/// T0–T2 re-key preserves it (§4.5.2, D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BufferId(u64);

impl BufferId {
    /// Generate a new unique buffer identity.
    #[must_use]
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// The numeric value of this identity.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl Default for BufferId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<u64> for BufferId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Monotonically increasing revision number for a buffer.
///
/// Every edit, undo, redo, reload, and journal patch advances the revision,
/// even if text returns to previous bytes (§4.5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Revision(u64);

impl Revision {
    /// Initial revision (0) for a newly opened buffer.
    pub const INITIAL: Self = Self(0);

    /// Create a new revision from a raw value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The numeric value of this revision.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Increment the revision by one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Check if this revision is after another.
    #[must_use]
    pub const fn is_after(self, other: Self) -> bool {
        self.0 > other.0
    }

    /// Check if this revision is before another.
    #[must_use]
    pub const fn is_before(self, other: Self) -> bool {
        self.0 < other.0
    }
}

impl Default for Revision {
    fn default() -> Self {
        Self::INITIAL
    }
}

/// A base hash representing the content hash of the last loaded or saved state.
///
/// Used for optimistic concurrency control: edits carry the expected base hash
/// of what they believe is on disk. A mismatch blocks the edit (§4.5.2, D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseHash([u8; 32]);

impl BaseHash {
    /// Create a base hash from blake3 hash bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create a base hash from a byte slice (blake3 output).
    ///
    /// # Panics
    /// Panics if the slice is not exactly 32 bytes.
    #[must_use]
    pub fn from_slice(slice: &[u8]) -> Self {
        let bytes: [u8; 32] = slice.try_into().expect("BaseHash must be 32 bytes");
        Self::from_bytes(bytes)
    }

    /// Get the hash as bytes.
    #[must_use]
    pub fn as_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// Get the hash as a byte slice.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Create a base hash from a TextSnapshot's content hash.
    #[must_use]
    pub fn from_snapshot(snapshot: &TextSnapshot) -> Self {
        use blake3::Hash;
        let hash = blake3::hash(snapshot.source());
        Self::from_bytes(hash.into())
    }
}

impl From<[u8; 32]> for BaseHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<BaseHash> for [u8; 32] {
    fn from(hash: BaseHash) -> Self {
        hash.0
    }
}

impl std::fmt::Display for BaseHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

/// Precondition for a revisioned edit operation.
///
/// Carries the buffer identity, the expected revision, and the expected base hash
/// (§4.5.2). The Session checks these when executing the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditPrecondition {
    /// The buffer this edit applies to.
    pub buffer_id: BufferId,
    /// The expected revision at the time the edit was issued.
    pub expected_revision: Revision,
    /// The expected base hash of the buffer's content on disk.
    pub expected_base_hash: Option<BaseHash>,
}

impl EditPrecondition {
    /// Create a new precondition for a buffer at a specific revision.
    #[must_use]
    pub fn new(buffer_id: BufferId, expected_revision: Revision) -> Self {
        Self {
            buffer_id,
            expected_revision,
            expected_base_hash: None,
        }
    }

    /// Create a precondition with an expected base hash.
    #[must_use]
    pub fn with_base_hash(
        buffer_id: BufferId,
        expected_revision: Revision,
        expected_base_hash: BaseHash,
    ) -> Self {
        Self {
            buffer_id,
            expected_revision,
            expected_base_hash: Some(expected_base_hash),
        }
    }
}

/// Result of checking a revisioned precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreconditionResult {
    /// The precondition is satisfied; the operation can proceed.
    Satisfied,
    /// The buffer identity does not match (buffer was reopened).
    WrongIdentity,
    /// The revision has changed since the operation was issued.
    StaleRevision,
    /// The base hash does not match what is on disk.
    StaleBaseHash,
}

/// Helper trait for types that can compute a content hash.
pub trait ContentHash {
    /// Compute the blake3 hash of this content.
    fn content_hash(&self) -> BaseHash;
}

impl ContentHash for TextSnapshot {
    fn content_hash(&self) -> BaseHash {
        BaseHash::from_snapshot(self)
    }
}

impl ContentHash for str {
    fn content_hash(&self) -> BaseHash {
        use blake3::Hash;
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.as_bytes());
        let hash = hasher.finalize();
        BaseHash::from_bytes(hash.into())
    }
}

impl ContentHash for [u8] {
    fn content_hash(&self) -> BaseHash {
        use blake3::Hash;
        let mut hasher = blake3::Hasher::new();
        hasher.update(self);
        let hash = hasher.finalize();
        BaseHash::from_bytes(hash.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_id_uniqueness() {
        let id1 = BufferId::new();
        let id2 = BufferId::new();
        assert_ne!(id1, id2);
        assert_eq!(id1, id1);
    }

    #[test]
    fn revision_ordering() {
        let r0 = Revision::INITIAL;
        let r1 = r0.next();
        let r2 = r1.next();

        assert!(r0 < r1);
        assert!(r1 < r2);
        assert!(r0.is_before(r1));
        assert!(r2.is_after(r1));
        assert!(!r0.is_after(r1));
    }

    #[test]
    fn base_hash_from_snapshot() {
        let snapshot = TextSnapshot::new(b"hello world").unwrap();
        let hash1 = BaseHash::from_snapshot(&snapshot);
        let hash2 = BaseHash::from_snapshot(&snapshot);
        assert_eq!(hash1, hash2);

        let snapshot2 = TextSnapshot::new(b"hello world!").unwrap();
        let hash3 = BaseHash::from_snapshot(&snapshot2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn base_hash_display() {
        let hash = BaseHash::from_bytes([0xAB; 32]);
        let display = format!("{}", hash);
        assert_eq!(display.len(), 64); // 32 bytes * 2 hex chars each
        assert!(display.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
