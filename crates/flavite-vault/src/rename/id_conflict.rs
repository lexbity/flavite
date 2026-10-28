//! Duplicate stable ID detection and resolution (D8, §4.8, §4.16.2).
//!
//! This module handles the detection, flagging, and resolution of duplicate
//! stable IDs in the vault.
//!
//! **Key Rule**: Duplicate frontmatter stable_id values are stored without a
//! UNIQUE constraint. Every raw frontmatter ID is retained. Every member of a
//! duplicate group is flagged, and the deterministic native PathKey winner
//! (lowest in byte order) is chosen for lookup while reporting ambiguity.
//! This rule is rebuild-stable.
//!
//! **T2 Constraint**: Only unconflicted IDs qualify for T2 identity transfer or
//! stable-ID pin keys. Conflicted notes use native path pins.

use crate::channels::{Notice, NoticeChannel};
use flavite_index::types::NoteRef as IndexNoteRef;
use std::collections::{HashMap, HashSet};

/// Information about a duplicate stable ID conflict.
#[derive(Debug, Clone)]
pub struct DuplicateIdInfo {
    /// The conflicting stable ID.
    pub stable_id: String,
    /// All index notes that share this stable ID.
    pub index_notes: Vec<IndexNoteRef>,
    /// The deterministic winner (lowest native PathKey in byte order).
    pub winner: IndexNoteRef,
    /// Whether this conflict is new (just detected).
    pub is_new: bool,
}

/// Result of duplicate ID resolution.
#[derive(Debug, Clone)]
pub struct DuplicateIdResolution {
    /// The stable ID that was resolved.
    pub stable_id: String,
    /// The old notes that had this ID.
    pub old_notes: Vec<IndexNoteRef>,
    /// The new note that got a fresh ID.
    pub new_note: IndexNoteRef,
    /// The newly minted stable ID.
    pub new_stable_id: String,
}

/// Manager for duplicate stable ID detection and resolution.
#[derive(Default)]
pub struct DuplicateIdManager {
    /// Track which stable IDs are currently in conflict.
    conflicted_ids: HashSet<String>,
    /// Track which stable IDs map to which index notes.
    stable_id_notes: HashMap<String, Vec<IndexNoteRef>>,
}

impl DuplicateIdManager {
    /// Create a new duplicate ID manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the manager with the current state of notes and their stable IDs.
    ///
    /// This should be called after every reconcile to keep the manager in sync.
    pub fn update_from_notes(&mut self, notes: &[flavite_index::types::Note]) {
        self.stable_id_notes.clear();
        self.conflicted_ids.clear();

        for note in notes {
            if let Some(stable_id) = &note.stable_id {
                let note_ref = IndexNoteRef::new(note.note_id, note.path_native.clone());

                // Track stable_id -> notes
                self.stable_id_notes
                    .entry(stable_id.clone())
                    .or_default()
                    .push(note_ref);
            }
        }

        // Identify conflicts
        for (stable_id, notes) in &self.stable_id_notes {
            if notes.len() > 1 {
                self.conflicted_ids.insert(stable_id.clone());
            }
        }
    }

    /// Detect duplicate stable IDs from a list of notes.
    ///
    /// Returns a map from each conflicting stable ID to its list of notes.
    #[must_use]
    pub fn detect_duplicates(&self) -> HashMap<String, Vec<IndexNoteRef>> {
        let mut result = HashMap::new();

        for (stable_id, notes) in &self.stable_id_notes {
            if notes.len() > 1 {
                result.insert(stable_id.clone(), notes.clone());
            }
        }

        result
    }

    /// Get the deterministic winner for a conflicting stable ID.
    ///
    /// The winner is the note with the lowest native PathKey in deterministic
    /// byte order. This rule is rebuild-stable.
    #[must_use]
    pub fn get_winner(&self, stable_id: &str) -> Option<IndexNoteRef> {
        self.stable_id_notes.get(stable_id).and_then(|notes| {
            if notes.len() > 1 {
                // Find the note with the lowest path_native in byte order
                notes
                    .iter()
                    .min_by(|a, b| a.path_native.cmp(&b.path_native))
                    .cloned()
            } else {
                notes.first().cloned()
            }
        })
    }

    /// Check if a stable ID is currently in conflict.
    #[must_use]
    pub fn is_conflicted(&self, stable_id: &str) -> bool {
        self.conflicted_ids.contains(stable_id)
    }

    /// Check if a note's stable ID is conflicted.
    #[must_use]
    pub fn note_has_conflicted_id(&self, note: &flavite_index::types::Note) -> bool {
        note.stable_id
            .as_ref()
            .map(|id| self.is_conflicted(id))
            .unwrap_or(false)
    }

    /// Mint a new stable ID for a note.
    ///
    /// This creates a new UUID-based stable ID for the given note.
    pub fn mint_new_stable_id(&self) -> String {
        // Generate a new ULID-style ID (sortable UUID)
        // For now, we use a simple UUIDv7
        uuid::Uuid::now_v7().to_string()
    }

    /// Resolve a duplicate ID by minting a new ID for one of the notes.
    ///
    /// This removes one note from the conflict group and assigns it a new ID.
    /// The winner of the original group remains with the original ID.
    ///
    /// # Arguments
    ///
    /// * `stable_id` - The conflicting stable ID to resolve
    /// * `note_to_fix` - The index note reference that should get a new ID
    /// * `notice_channel` - Channel to send notices to
    ///
    /// # Returns
    ///
    /// The resolution information, or None if the stable_id is not in conflict.
    pub fn resolve_duplicate(
        &mut self,
        stable_id: &str,
        note_to_fix: &IndexNoteRef,
        notice_channel: &NoticeChannel,
    ) -> Option<DuplicateIdResolution> {
        // Check if this is actually a conflict
        let notes = self.stable_id_notes.get(stable_id)?;
        if notes.len() <= 1 {
            return None;
        }

        // Check if the note_to_fix actually has this stable_id
        let has_note = notes.iter().any(|n| n.note_id == note_to_fix.note_id);
        if !has_note {
            return None;
        }

        // Mint a new stable ID
        let new_stable_id = self.mint_new_stable_id();

        // Create the resolution
        let resolution = DuplicateIdResolution {
            stable_id: stable_id.to_string(),
            old_notes: notes.clone(),
            new_note: note_to_fix.clone(),
            new_stable_id: new_stable_id.clone(),
        };

        // Update our state
        // Remove the note from the old stable_id
        if let Some(notes_list) = self.stable_id_notes.get_mut(stable_id) {
            notes_list.retain(|n| n.note_id != note_to_fix.note_id);
            if notes_list.len() <= 1 {
                self.conflicted_ids.remove(stable_id);
            }
        }

        // Add the note to the new stable_id
        self.stable_id_notes
            .entry(new_stable_id)
            .or_default()
            .push(note_to_fix.clone());

        // Send a notice about the resolution
        notice_channel.send(Notice::ActionRequired {
            code: "duplicate_id_resolved",
            note: None, // We don't have a buffer NoteRef here, using index NoteRef
        });

        Some(resolution)
    }

    /// Get information about all duplicate stable IDs.
    #[must_use]
    pub fn get_duplicate_info(&self) -> Vec<DuplicateIdInfo> {
        let mut result = Vec::new();

        for (stable_id, notes) in &self.stable_id_notes {
            if notes.len() > 1 {
                let winner = self.get_winner(stable_id);

                result.push(DuplicateIdInfo {
                    stable_id: stable_id.clone(),
                    index_notes: notes.clone(),
                    winner: winner.unwrap_or_else(|| notes[0].clone()),
                    is_new: false, // We don't track "new" state in this simple implementation
                });
            }
        }

        result
    }
}

/// Check if a note can use T2 identity transfer.
///
/// Only unconflicted IDs qualify for T2 identity transfer or stable-ID pin keys.
/// Conflicted notes use native path pins.
#[must_use]
pub fn can_use_t2_identity(
    note: &flavite_index::types::Note,
    duplicate_manager: &DuplicateIdManager,
) -> bool {
    note.stable_id
        .as_ref()
        .map(|id| !duplicate_manager.is_conflicted(id))
        .unwrap_or(false)
}

/// Check if a rename should carry identity state silently.
///
/// Identity state (buffer re-key, pin carry-over) is only silent for T0, T1, and T2.
/// T3 and T4 require user confirmation and do NOT carry identity state.
#[must_use]
pub fn should_carry_identity(tier: crate::channels::RenameTier) -> bool {
    matches!(
        tier,
        crate::channels::RenameTier::T0
            | crate::channels::RenameTier::T1
            | crate::channels::RenameTier::T2
    )
}

/// Check if a rename should auto-rewrite links.
///
/// Only T0 (in-app) renames auto-rewrite links. All others emit a notice
/// and let the user decide.
#[must_use]
pub fn should_auto_rewrite_links(tier: crate::channels::RenameTier) -> bool {
    matches!(tier, crate::channels::RenameTier::T0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_index_note_ref(path: &str, note_id: i64) -> IndexNoteRef {
        IndexNoteRef::new(note_id, path.as_bytes().to_vec())
    }

    fn mock_note(path: &str, stable_id: Option<&str>, note_id: i64) -> flavite_index::types::Note {
        flavite_index::types::Note {
            note_id,
            path: path.to_string(),
            path_native: path.as_bytes().to_vec(),
            path_key: path.to_lowercase(),
            stem_key: std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .to_lowercase(),
            title: path.to_string(),
            stable_id: stable_id.map(|s| s.to_string()),
            id_conflict: false,
            content_hash: vec![],
            size: 0,
            mtime_ns: 0,
            scan_ns: 0,
            frontmatter: None,
            preview: None,
            parse_ok: true,
            generation: 0,
        }
    }

    #[test]
    fn test_duplicate_detection() {
        let mut manager = DuplicateIdManager::new();

        // Create notes with duplicate stable IDs
        let stable_id = "test-id".to_string();
        let note1 = mock_note("note1.md", Some(&stable_id), 1);
        let note2 = mock_note("note2.md", Some(&stable_id), 2);

        manager.update_from_notes(&[note1, note2]);

        let duplicates = manager.detect_duplicates();
        assert!(duplicates.contains_key(&stable_id));
        assert_eq!(duplicates[&stable_id].len(), 2);
    }

    #[test]
    fn duplicate_resolution_updates_indexes_and_reports_edge_cases() {
        use crate::channels::ChannelSet;

        let mut manager = DuplicateIdManager::new();
        let one = mock_note("a.md", Some("duplicate"), 1);
        let two = mock_note("b.md", Some("duplicate"), 2);
        let unique = mock_note("c.md", Some("unique"), 3);
        let missing_id = mock_note("d.md", None, 4);
        manager.update_from_notes(&[one.clone(), two.clone(), unique.clone(), missing_id.clone()]);
        assert!(manager.note_has_conflicted_id(&one));
        assert!(!manager.note_has_conflicted_id(&unique));
        assert!(!manager.note_has_conflicted_id(&missing_id));
        assert_eq!(manager.get_duplicate_info().len(), 1);
        assert!(manager.get_winner("unique").is_some());
        assert!(manager.get_winner("absent").is_none());

        let channels = ChannelSet::new();
        assert!(
            manager
                .resolve_duplicate("absent", &one_ref(1), &channels.notice)
                .is_none()
        );
        assert!(
            manager
                .resolve_duplicate("duplicate", &one_ref(99), &channels.notice)
                .is_none()
        );
        let resolved = manager
            .resolve_duplicate("duplicate", &one_ref(2), &channels.notice)
            .unwrap();
        assert_eq!(resolved.old_notes.len(), 2);
        assert_ne!(resolved.new_stable_id, "duplicate");
        assert!(!manager.is_conflicted("duplicate"));
        assert_eq!(manager.detect_duplicates().len(), 0);
        assert_eq!(manager.get_duplicate_info().len(), 0);
        assert!(matches!(
            channels.notice.try_recv().unwrap(),
            crate::channels::Notice::ActionRequired {
                code: "duplicate_id_resolved",
                ..
            }
        ));
    }

    fn one_ref(note_id: i64) -> IndexNoteRef {
        IndexNoteRef::new(note_id, format!("{note_id}.md").into_bytes())
    }

    #[test]
    fn test_winner_selection() {
        let mut manager = DuplicateIdManager::new();

        let stable_id = "test-id".to_string();
        let note1 = mock_note("a/note.md", Some(&stable_id), 1);
        let note2 = mock_note("z/note.md", Some(&stable_id), 2);

        manager.update_from_notes(&[note1, note2]);

        let winner = manager.get_winner(&stable_id);
        assert!(winner.is_some());

        // The winner should be the one with the lowest path_native in byte order
        // "a/note.md" < "z/note.md" lexicographically
        let winner_ref = winner.unwrap();
        assert_eq!(winner_ref.note_id, 1);
    }

    #[test]
    fn test_identity_carry_rules() {
        use crate::channels::RenameTier;

        assert!(should_carry_identity(RenameTier::T0));
        assert!(should_carry_identity(RenameTier::T1));
        assert!(should_carry_identity(RenameTier::T2));
        assert!(!should_carry_identity(RenameTier::T3));
        assert!(!should_carry_identity(RenameTier::T4));
    }

    #[test]
    fn test_auto_rewrite_rules() {
        use crate::channels::RenameTier;

        assert!(should_auto_rewrite_links(RenameTier::T0));
        assert!(!should_auto_rewrite_links(RenameTier::T1));
        assert!(!should_auto_rewrite_links(RenameTier::T2));
        assert!(!should_auto_rewrite_links(RenameTier::T3));
        assert!(!should_auto_rewrite_links(RenameTier::T4));
    }

    #[test]
    fn test_mint_new_id() {
        let manager = DuplicateIdManager::new();
        let id1 = manager.mint_new_stable_id();
        let id2 = manager.mint_new_stable_id();

        // IDs should be unique
        assert_ne!(id1, id2);

        // IDs should be valid UUIDs
        assert!(uuid::Uuid::parse_str(&id1).is_ok());
        assert!(uuid::Uuid::parse_str(&id2).is_ok());
    }

    #[test]
    fn test_can_use_t2_identity() {
        let mut manager = DuplicateIdManager::new();

        let stable_id = "test-id".to_string();
        let note1 = mock_note("note1.md", Some(&stable_id), 1);
        let note2 = mock_note("note2.md", Some(&stable_id), 2);
        let note3 = mock_note("note3.md", Some("unique-id"), 3);

        let notes = vec![note1.clone(), note2.clone(), note3.clone()];
        manager.update_from_notes(&notes);

        // note3 has a unique ID, so it can use T2
        assert!(can_use_t2_identity(&note3, &manager));

        // note1 and note2 have a conflicting ID, so they cannot
        assert!(!can_use_t2_identity(&note1, &manager));
        assert!(!can_use_t2_identity(&note2, &manager));
    }
}
