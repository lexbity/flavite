//! Bounded query read models for workspace shells (RFC-0003 W02).
//!
//! The UI never touches the index, SQLite, or the filesystem directly; it
//! consumes these typed snapshots through the facade. Snapshot failures are
//! real loading/retry states, not silent empty results.

use crate::{Flavite, FlaviteError};
use std::path::PathBuf;

/// Largest catalog page the facade admits (NFR-02: quick switch over 50k
/// titles). Callers page beyond this bound.
pub const MAX_NOTE_CATALOG_LIMIT: usize = 50_000;

/// Reserved requesting-context identity for ticketed catalog acquisition
/// (RFC-0004 D9). No workspace view ever owns tab id [`u128::MAX`], so
/// catalog tickets can never collide with a tab's context/search queries;
/// supersession is per-`(tab, incarnation, purpose)` and therefore
/// catalog-only.
pub const CATALOG_CONTEXT_TAB: u128 = u128::MAX;

/// One indexed note with native path identity preserved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NoteCatalogEntry {
    /// Portable vault-relative path.
    pub path: PathBuf,
    /// Exact native path bytes as stored by the index (authoritative
    /// identity; display strings are not identity).
    pub native_path: Vec<u8>,
    /// Indexed title.
    pub title: String,
}

/// A bounded note catalog page with the index generation it came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NoteCatalog {
    /// Entries in canonical path order.
    pub entries: Vec<NoteCatalogEntry>,
    /// Index epoch backing this page.
    pub epoch: u64,
    /// Index generation backing this page.
    pub generation: u64,
    /// Total indexed notes; may exceed `entries.len()`.
    pub total: u64,
}

impl Flavite {
    /// Read a bounded native-identity note catalog page for the Notes view
    /// and the note picker.
    ///
    /// # Blocking contract
    /// Blocks the calling thread for a bounded catalog read (a 50k-row
    /// round trip through the vault actor). MUST NOT be called from a UI
    /// host turn — hosts use [`Flavite::submit_catalog`] and apply the
    /// drained outcome; a `just gate` grep enforces this in
    /// `crates/flavite-ui/src`. Retained for CLI and tests.
    ///
    /// # Errors
    /// Returns [`FlaviteError::IndexError`] while the index is rebuilding
    /// or unavailable; hosts render a retry state, not an empty vault.
    pub fn note_catalog(&self, limit: usize) -> Result<NoteCatalog, FlaviteError> {
        let limit = limit.min(MAX_NOTE_CATALOG_LIMIT);
        let page = self
            .session
            .note_catalog(limit)
            .map_err(FlaviteError::IndexError)?;
        Ok(NoteCatalog {
            entries: page
                .items
                .into_iter()
                .map(|item| NoteCatalogEntry {
                    path: PathBuf::from(item.path),
                    native_path: item.path_native,
                    title: item.title,
                })
                .collect(),
            epoch: page.epoch,
            generation: page.generation,
            total: page.total,
        })
    }

    /// Submit the bounded note catalog as a query-service ticket
    /// (RFC-0004 W-D, D9). The whole read runs on a query worker; the host
    /// turn pays only the admission. Outcomes arrive through
    /// [`crate::QueryService::drain`] as
    /// [`ContextQueryResult::Catalog`](crate::ContextQueryResult::Catalog),
    /// stamped with `(epoch, generation)` provenance. The context id is the
    /// reserved [`CATALOG_CONTEXT_TAB`] and the sequence is managed
    /// internally, so a newer submit supersedes any older pending result
    /// and refresh triggers coalesce.
    ///
    /// # Errors
    /// Returns [`QuerySubmitError::Busy`](crate::QuerySubmitError::Busy)
    /// when every service slot is occupied; the caller keeps its last
    /// ready snapshot and retries on the next trigger.
    pub fn submit_catalog(&self) -> Result<crate::RequestToken, crate::QuerySubmitError> {
        let binding_sequence = self
            .catalog_sequence
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.queries.submit(
            CATALOG_CONTEXT_TAB,
            1,
            binding_sequence,
            crate::QueryPayload::Catalog {
                limit: MAX_NOTE_CATALOG_LIMIT as u64,
            },
        )
    }
}

impl crate::Flavite {
    /// Immutable resolver version for rejecting obsolete reading actions.
    /// This reads the published catalog only; it performs no actor/DB IO.
    #[must_use]
    pub fn read_catalog_version(&self) -> Option<(u64, u64)> {
        self.session
            .resolver_catalog()
            .ok()
            .map(|catalog| (catalog.epoch, catalog.generation))
    }

    /// Current shared buffer for an exact vault-relative source path, if open.
    pub fn open_source_view(&self, path: &std::path::Path) -> Option<crate::SharedBufferView> {
        let (id, revision, ..) =
            self.session
                .graph_buffer_stamp()
                .into_iter()
                .find(|(_, _, candidate, ..)| {
                    candidate
                        .strip_prefix(self.session.root())
                        .unwrap_or(candidate)
                        == path
                })?;
        self.get_shared_buffer_view(id, Some(revision)).ok()
    }
}
