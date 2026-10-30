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
}
