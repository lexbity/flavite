//! Opaque persistent identity for workspace entities (RFC-0003 §7.1).
//!
//! `GroupId`, `SplitId`, and `TabId` are opaque 128-bit identifiers. The
//! adapter treats iced pane/split ids as disposable keys and never derives
//! workspace identity from them. Allocation is monotonic inside one
//! [`Ids`] generator, so a seeded generator reproduces a whole session
//! deterministically (NFR-19) without global mutable state.

use std::fmt;

macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u128);

        impl $name {
            /// Wrap an already allocated opaque value. Callers that do not
            /// own the generator (deserialization, tests) use this; the
            /// constructor stays `pub` because identity equality is the
            /// only meaning these values have.
            #[must_use]
            pub const fn from_raw(value: u128) -> Self {
                Self(value)
            }

            /// The opaque value. Display strings are not identity.
            #[must_use]
            pub const fn raw(self) -> u128 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "-{:032x}"), self.0)
            }
        }
    };
}

opaque_id!(
    /// Identity of one workspace (persisted with its layout, RFC-0003 §7.1).
    WorkspaceId,
    "ws"
);
opaque_id!(
    /// Identity of one movable tab group; stable across moves and restarts.
    GroupId,
    "grp"
);
opaque_id!(
    /// Identity of one split node in the layout tree.
    SplitId,
    "spl"
);
opaque_id!(
    /// Identity of one tab; stable across moves within a session.
    TabId,
    "tab"
);

/// Monotonic allocator for workspace identities.
///
/// The first value mixes a caller-supplied seed so separate processes do not
/// mint identical id sequences, while remaining fully reproducible for a
/// given seed (property tests and fuzzers use fixed seeds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ids {
    next: u128,
}

impl Ids {
    /// A generator seeded from OS entropy sources available without I/O:
    /// the coarse clock and the allocator address of the generator itself.
    #[must_use]
    pub fn new() -> Self {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_u128, |elapsed| elapsed.as_nanos());
        // The process id only decorrelates same-nanosecond generators; it
        // never becomes observable identity.
        let decorrelate = u128::from(std::process::id()) << 96;
        Self::with_seed(time.rotate_left(64) ^ decorrelate)
    }

    /// Advance the generator past an observed identity so freshly allocated
    /// ids never collide with restored persistent ones (W06 restore).
    pub fn observe(&mut self, value: u128) {
        if value >= self.next {
            self.next = value.saturating_add(1);
        }
    }

    /// A reproducible generator for tests and fuzz targets.
    #[must_use]
    pub fn with_seed(seed: u128) -> Self {
        // SplitMix64-style avalanche so small seeds spread across the space.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self { next: z }
    }

    fn allocate(&mut self) -> u128 {
        // Checked; a u128 counter cannot realistically overflow, and a wrap
        // must never silently reuse identity.
        self.next = self
            .next
            .checked_add(1)
            .expect("workspace id space exhausted");
        self.next
    }

    /// Allocate a workspace identity.
    pub fn workspace_id(&mut self) -> WorkspaceId {
        WorkspaceId::from_raw(self.allocate())
    }

    /// Allocate a group identity.
    pub fn group_id(&mut self) -> GroupId {
        GroupId::from_raw(self.allocate())
    }

    /// Allocate a split identity.
    pub fn split_id(&mut self) -> SplitId {
        SplitId::from_raw(self.allocate())
    }

    /// Allocate a tab identity.
    pub fn tab_id(&mut self) -> TabId {
        TabId::from_raw(self.allocate())
    }
}

impl Default for Ids {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_generators_are_deterministic_and_distinct() {
        let mut a = Ids::with_seed(7);
        let mut b = Ids::with_seed(7);
        let mut c = Ids::with_seed(8);
        for _ in 0..4 {
            assert_eq!(a.tab_id(), b.tab_id());
        }
        assert_ne!(a.tab_id(), c.tab_id());
    }

    #[test]
    fn ids_render_as_prefixed_opaque_hex() {
        let mut ids = Ids::with_seed(1);
        let tab = ids.tab_id();
        let rendered = tab.to_string();
        assert!(rendered.starts_with("tab-"), "{rendered}");
        assert_eq!(rendered.len(), 4 + 32);
    }
}
