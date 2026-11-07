//! Per-note source leases (RFC-0006 D-10/§6.9).
//!
//! One per-note mutation lease serializes Session edits, save, rename and
//! replacement-job operations from verification through durable outcome. It
//! is bounded by the in-flight IO result, never held for preview. This is
//! deliberately distinct from the self-write classification tokens
//! (`tokens.rs`): acquisition is non-replacing (a second acquire on the
//! same source is refused, not silently replacing the earlier owner), and
//! multi-source acquisition is one nonblocking all-or-none operation in
//! ascending native-key order. Stale releases are no-ops.

use crate::sync::RwLockExt;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The typed mutation owner of a source lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseOwner {
    /// Buffer editing (typed edits).
    Edit,
    /// Save/autosave.
    Save,
    /// Journaled rename / inbound-link patch.
    Rename,
    /// Durable replacement job item.
    Replace,
}

impl LeaseOwner {
    /// Stable, redacted user-facing label (NFR-17).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Save => "save",
            Self::Rename => "rename",
            Self::Replace => "replacement",
        }
    }
}

/// A held source lease. `release` is a no-op when it is no longer the live
/// lease for its key (stale release never revokes a newer owner).
#[derive(Debug, Clone)]
pub struct SourceLease {
    key: Vec<u8>,
    owner: LeaseOwner,
    generation: u64,
}

impl SourceLease {
    /// The canonical source key this lease covers.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The typed owner.
    #[must_use]
    pub const fn owner(&self) -> LeaseOwner {
        self.owner
    }

    /// The unique owner/generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Lease registry state.
#[derive(Debug, Default)]
struct LeaseState {
    by_key: HashMap<Vec<u8>, (LeaseOwner, u64)>,
}

/// Non-replacing per-source lease registry (RFC-0006 §6.9).
#[derive(Debug)]
pub struct SourceLeaseRegistry {
    state: RwLock<LeaseState>,
    next_generation: AtomicU64,
}

impl Default for SourceLeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceLeaseRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: RwLock::new(LeaseState::default()),
            next_generation: AtomicU64::new(1),
        }
    }

    /// Acquire one lease non-replacing. `Err` means the source is already
    /// leased (Busy) — never a silent takeover.
    #[allow(clippy::result_unit_err)]
    pub fn acquire(&self, key: Vec<u8>, owner: LeaseOwner) -> Result<SourceLease, ()> {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.write_recovering();
        if state.by_key.contains_key(&key) {
            return Err(());
        }
        state.by_key.insert(key.clone(), (owner, generation));
        Ok(SourceLease {
            key,
            owner,
            generation,
        })
    }

    /// Acquire several sources as one nonblocking all-or-none operation in
    /// ascending native-key order (RFC-0006 §6.9). On failure nothing is
    /// held and the first conflicting key is returned.
    pub fn acquire_many(
        &self,
        wants: &[(Vec<u8>, LeaseOwner)],
    ) -> Result<Vec<SourceLease>, Vec<u8>> {
        let mut ordered = wants.to_vec();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        let generation_base = self.next_generation.fetch_add(
            u64::try_from(ordered.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let mut state = self.state.write_recovering();
        let mut held: Vec<SourceLease> = Vec::with_capacity(ordered.len());
        for (index, (key, owner)) in ordered.into_iter().enumerate() {
            if state.by_key.contains_key(&key) {
                for lease in &held {
                    state.by_key.remove(&lease.key);
                }
                return Err(key);
            }
            let generation = generation_base.saturating_add(index as u64);
            state.by_key.insert(key.clone(), (owner, generation));
            held.push(SourceLease {
                key,
                owner,
                generation,
            });
        }
        Ok(held)
    }

    /// Release a lease. Stale releases (a newer owner holds the key) are
    /// silent no-ops.
    pub fn release(&self, lease: &SourceLease) {
        let mut state = self.state.write_recovering();
        if state.by_key.get(&lease.key) == Some(&(lease.owner, lease.generation)) {
            state.by_key.remove(&lease.key);
        }
    }

    /// Whether a source is currently leased, and by whom.
    #[must_use]
    pub fn is_leased(&self, key: &[u8]) -> Option<(LeaseOwner, u64)> {
        let state = self.state.read_recovering();
        state.by_key.get(key).copied()
    }

    /// Whether a source is leased by a specific owner.
    #[must_use]
    pub fn is_leased_by(&self, key: &[u8], owner: LeaseOwner) -> bool {
        self.is_leased(key).is_some_and(|(held, _)| held == owner)
    }

    /// The number of live leases (diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.read_recovering().by_key.len()
    }

    /// Whether any lease is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> Vec<u8> {
        value.as_bytes().to_vec()
    }

    #[test]
    fn acquisition_is_non_replacing_and_release_is_safe() {
        let registry = SourceLeaseRegistry::new();
        let first = registry.acquire(key("a.md"), LeaseOwner::Replace).unwrap();
        assert!(registry.acquire(key("a.md"), LeaseOwner::Edit).is_err());
        assert_eq!(
            registry.is_leased(b"a.md"),
            Some((LeaseOwner::Replace, first.generation()))
        );
        // Stale release from the replaced-owner position is a no-op; the
        // live owner is untouched.
        let stale = SourceLease {
            key: key("a.md"),
            owner: LeaseOwner::Edit,
            generation: first.generation().saturating_add(1),
        };
        registry.release(&stale);
        assert!(registry.is_leased(b"a.md").is_some());
        registry.release(&first);
        assert!(registry.is_leased(b"a.md").is_none());
    }

    #[test]
    fn multi_source_acquisition_is_all_or_none_in_ascending_order() {
        let registry = SourceLeaseRegistry::new();
        let wants = vec![
            (key("z.md"), LeaseOwner::Replace),
            (key("a.md"), LeaseOwner::Replace),
            (key("m.md"), LeaseOwner::Replace),
        ];
        let held = registry.acquire_many(&wants).expect("acquire all");
        assert_eq!(held.len(), 3);
        // Keys were locked in ascending order.
        assert_eq!(held[0].key(), b"a.md");
        assert_eq!(held[1].key(), b"m.md");
        assert_eq!(held[2].key(), b"z.md");
        for lease in &held {
            assert!(registry.is_leased(lease.key()).is_some());
        }
        // A partially-conflicting acquire releases everything.
        let conflict = vec![
            (key("b.md"), LeaseOwner::Edit),
            (key("a.md"), LeaseOwner::Edit),
        ];
        let err = registry.acquire_many(&conflict).expect_err("conflict");
        assert_eq!(err, b"a.md");
        assert!(registry.is_leased(b"b.md").is_none());
        assert!(
            registry.is_leased(b"a.md").is_some(),
            "original owner remains"
        );
    }

    #[test]
    fn owners_are_typed_and_distinct() {
        let registry = SourceLeaseRegistry::new();
        let edit = registry.acquire(key("x.md"), LeaseOwner::Edit).unwrap();
        assert_eq!(edit.owner(), LeaseOwner::Edit);
        assert_eq!(LeaseOwner::Replace.label(), "replacement");
        assert!(registry.is_leased_by(b"x.md", LeaseOwner::Edit));
        assert!(!registry.is_leased_by(b"x.md", LeaseOwner::Save));
    }
}
