//! Core types for the vault subsystem (§4.1, §4.2, §4.5).
//!
//! This module contains the fundamental types used throughout the vault:
//! epoch/generation counters, vault references, scope definitions, and
//! reconcile modes.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Global epoch counter for index versions (§4.14).
///
/// An epoch is created when SQLite is recreated (e.g., after corruption).
/// Generations are monotonic only within an epoch.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Epoch(pub u64);

impl Epoch {
    /// Create a new epoch.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Get the epoch value.
    #[must_use]
    pub const fn value(&self) -> u64 {
        self.0
    }

    /// Increment the epoch.
    #[must_use]
    pub const fn next(&self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "epoch:{}", self.0)
    }
}

/// Generation counter within an epoch (§4.14).
///
/// Bumps on every structural write transaction. Used for invalidation
/// tracking and search checkpoint alignment.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Generation(pub u64);

impl Generation {
    /// Create a new generation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Get the generation value.
    #[must_use]
    pub const fn value(&self) -> u64 {
        self.0
    }

    /// Increment the generation.
    #[must_use]
    pub const fn next(&self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gen:{}", self.0)
    }
}

/// A version identifier combining epoch and generation (§4.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Version {
    pub epoch: Epoch,
    pub generation: Generation,
}

impl Version {
    /// Create a new version.
    #[must_use]
    pub const fn new(epoch: Epoch, generation: Generation) -> Self {
        Self { epoch, generation }
    }

    /// Create a version from raw values.
    #[must_use]
    pub const fn from_values(epoch: u64, generation: u64) -> Self {
        Self {
            epoch: Epoch::new(epoch),
            generation: Generation::new(generation),
        }
    }

    /// Increment the generation within the same epoch.
    #[must_use]
    pub const fn next_generation(&self) -> Self {
        Self {
            epoch: self.epoch,
            generation: self.generation.next(),
        }
    }

    /// Start a new epoch (resets generation to 0).
    #[must_use]
    pub const fn new_epoch(&self) -> Self {
        Self {
            epoch: self.epoch.next(),
            generation: Generation::new(0),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.epoch, self.generation)
    }
}

/// Atomic version for safe concurrent access.
pub struct AtomicVersion {
    epoch: AtomicU64,
    generation: AtomicU64,
}

impl AtomicVersion {
    /// Create a new atomic version.
    #[must_use]
    pub const fn new(epoch: u64, generation: u64) -> Self {
        Self {
            epoch: AtomicU64::new(epoch),
            generation: AtomicU64::new(generation),
        }
    }

    /// Load the current version.
    #[must_use]
    pub fn load(&self, ordering: Ordering) -> Version {
        Version::from_values(self.epoch.load(ordering), self.generation.load(ordering))
    }

    /// Store a new version.
    pub fn store(&self, version: Version, ordering: Ordering) {
        self.epoch.store(version.epoch.value(), ordering);
        self.generation.store(version.generation.value(), ordering);
    }

    /// Increment the generation and return the new version.
    #[must_use]
    pub fn increment_generation(&self, ordering: Ordering) -> Version {
        let epoch = self.epoch.load(ordering);
        let generation = self.generation.fetch_add(1, ordering);
        Version::from_values(epoch, generation)
    }

    /// Start a new epoch and return the new version.
    #[must_use]
    pub fn new_epoch(&self, ordering: Ordering) -> Version {
        let old_epoch = self.epoch.fetch_add(1, ordering);
        self.generation.store(0, ordering);
        Version::from_values(old_epoch + 1, 0)
    }
}

impl Default for AtomicVersion {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

/// Reference to a vault by its root path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VaultRef {
    /// The root path of the vault.
    pub root: PathBuf,
}

impl VaultRef {
    /// Create a new vault reference.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Scope for reconcile operations (§4.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReconcileScope {
    /// Only the specified path.
    Single(PathBuf),
    /// All notes in the vault.
    Full,
    /// Notes in a specific directory.
    Directory(PathBuf),
}

/// Mode for reconcile operations (§4.4, D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReconcileMode {
    /// Fast metadata scan (size + mtime only).
    ///
    /// Skips content hashing for unchanged files. Uses the racy-file rule
    /// (mtime ≥ scan_time − 2s) to force hashing.
    FastMetadata,
    /// Full content verification.
    ///
    /// Hashes every eligible file in scope regardless of size/mtime equality.
    /// This is the backstop against silent loss.
    VerifyContent,
}

/// Kind of change detected during reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// File was created.
    Created,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
    /// File was renamed (detected via inference).
    Renamed,
}

/// Summary statistics for a reconcile operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileSummary {
    /// Number of notes added.
    pub added: u32,
    /// Number of notes changed.
    pub changed: u32,
    /// Number of notes removed.
    pub removed: u32,
}

impl ReconcileSummary {
    /// Create a new summary.
    #[must_use]
    pub const fn new(added: u32, changed: u32, removed: u32) -> Self {
        Self {
            added,
            changed,
            removed,
        }
    }

    /// Total number of changes.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.added + self.changed + self.removed
    }

    /// Whether there were any changes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Configuration for the vault.
#[derive(Debug, Clone)]
pub struct VaultConfig {
    /// Whether to enable watcher events.
    pub watcher_enabled: bool,
    /// Interval for periodic verification (default: 10 minutes).
    pub verification_interval: Duration,
    /// Quiet window for event coalescing (default: 300ms).
    pub coalesce_quiet_window: Duration,
    /// Maximum coalescing window (default: 2s).
    pub coalesce_max_window: Duration,
    /// Threshold for bulk mode (events per second, default: 500).
    pub bulk_threshold: u32,
    /// Capacity for the pulse channel (default: 1024).
    pub pulse_channel_capacity: usize,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            watcher_enabled: true,
            verification_interval: Duration::from_secs(600), // 10 minutes
            coalesce_quiet_window: Duration::from_millis(300),
            coalesce_max_window: Duration::from_secs(2),
            bulk_threshold: 500,
            pulse_channel_capacity: 1024,
        }
    }
}

/// Identity information for a vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultIdentity {
    /// The vault's unique identifier.
    pub uuid: uuid::Uuid,
    /// The root path of the vault.
    pub root: PathBuf,
    /// The native root key (for identity binding, §4.13).
    pub root_key: Vec<u8>,
}

impl VaultIdentity {
    /// Create a new vault identity.
    #[must_use]
    pub fn new(uuid: uuid::Uuid, root: PathBuf, root_key: Vec<u8>) -> Self {
        Self {
            uuid,
            root,
            root_key,
        }
    }
}
