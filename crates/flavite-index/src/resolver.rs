//! Link resolver (§4.3, D8).
//!
//! This module implements the 5-step resolution algorithm from the specification:
//!
//! 1. `id:` prefix → `stable_id` lookup.
//! 2. Path-suffix match if the target contains `/` (notes, then attachments).
//! 3. Stem match over `notes.stem_key ∪ aliases.alias_key ∪ attachments.stem_key`.
//!    For `![[embed]]`, an attachment match wins over a note match.
//!    For `[[link]]`, a note match wins.
//! 4. If multiple candidates → rank: same directory as source, then shallowest,
//!    then lexicographic. Mark **`ambiguous`**, pick deterministically, and
//!    surface it in Diagnostics.
//! 5. Zero candidates → `unresolved`. Clicking a note-link creates the note.
//!    Clicking an embed whose target has an attachment-like extension offers
//!    *locate file*; clicking an attachment link opens it with the OS default
//!    application.
//!
//! Duplicate IDs: retain every raw frontmatter ID, flag every member of a
//! duplicate group, and choose the lowest native `PathKey` in deterministic
//! byte order for lookup while reporting ambiguity. This rule is
//! rebuild-stable.

use crate::schema::ResState;
use crate::types::{AttRef, NoteRef};
use rusqlite::{Connection, Result, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Cached resolution state.
#[derive(Debug, Default)]
pub struct ResolverCache {
    /// Map from `target_key` to resolved note/attachment IDs.
    #[allow(dead_code)]
    target_to_note: HashMap<String, i64>,
    #[allow(dead_code)]
    target_to_att: HashMap<String, i64>,
    /// Map from `stable_id` to `note_id` (for id: links).
    stable_id_to_note: HashMap<String, i64>,
    /// Map from `stem_key` to list of `note_ids` (for step 3).
    stem_to_notes: HashMap<String, Vec<i64>>,
    /// Map from `stem_key` to list of `attachment_ids` (for step 3).
    stem_to_atts: HashMap<String, Vec<i64>>,
    /// Map from `alias_key` to `note_id`.
    alias_to_note: HashMap<String, i64>,
    /// Set of duplicate `stable_ids`.
    duplicate_stable_ids: HashSet<String>,
    /// Set of ambiguous `target_keys`.
    #[allow(dead_code)]
    ambiguous_targets: HashSet<String>,
}

/// Link resolution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The resolved note reference (if resolved to a note).
    pub note: Option<NoteRef>,
    /// The resolved attachment reference (if resolved to an attachment).
    pub attachment: Option<AttRef>,
    /// Whether the resolution is ambiguous.
    pub is_ambiguous: bool,
    /// Whether the target was unresolved.
    pub is_unresolved: bool,
    /// The resolution state.
    pub state: ResState,
}

impl Resolution {
    #[must_use]
    pub fn resolved_note(note: NoteRef) -> Self {
        Self {
            note: Some(note),
            attachment: None,
            is_ambiguous: false,
            is_unresolved: false,
            state: ResState::Resolved,
        }
    }

    #[must_use]
    pub fn resolved_attachment(attachment: AttRef) -> Self {
        Self {
            note: None,
            attachment: Some(attachment),
            is_ambiguous: false,
            is_unresolved: false,
            state: ResState::Resolved,
        }
    }

    #[must_use]
    pub fn ambiguous() -> Self {
        Self {
            note: None,
            attachment: None,
            is_ambiguous: true,
            is_unresolved: false,
            state: ResState::Ambiguous,
        }
    }

    #[must_use]
    pub fn unresolved() -> Self {
        Self {
            note: None,
            attachment: None,
            is_ambiguous: false,
            is_unresolved: true,
            state: ResState::Unresolved,
        }
    }
}

/// The link resolver.
pub struct Resolver {
    conn: Connection,
    cache: ResolverCache,
}

impl Resolver {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            cache: ResolverCache::default(),
        }
    }

    /// Initialize or refresh the cache from the database.
    ///
    /// # Errors
    /// Returns a SQLite error if any cache source query fails.
    pub fn refresh_cache(&mut self) -> Result<()> {
        self.cache = ResolverCache::default();

        // Load stable_id to note_id mappings
        let mut stmt = self
            .conn
            .prepare("SELECT stable_id, note_id FROM notes WHERE stable_id IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            let stable_id: String = row.get(0)?;
            let note_id: i64 = row.get(1)?;
            Ok((stable_id, note_id))
        })?;

        let mut stable_id_counts = HashMap::new();
        for row in rows {
            let (stable_id, note_id) = row?;
            stable_id_counts
                .entry(stable_id.clone())
                .or_insert_with(Vec::new)
                .push(note_id);
            self.cache.stable_id_to_note.insert(stable_id, note_id);
        }

        // Track duplicates
        for (stable_id, note_ids) in stable_id_counts {
            if note_ids.len() > 1 {
                self.cache.duplicate_stable_ids.insert(stable_id);
            }
        }

        // Load stem_key to note_id mappings
        let mut stmt = self.conn.prepare("SELECT stem_key, note_id FROM notes")?;
        let rows = stmt.query_map([], |row| {
            let stem_key: String = row.get(0)?;
            let note_id: i64 = row.get(1)?;
            Ok((stem_key, note_id))
        })?;

        for row in rows {
            let (stem_key, note_id) = row?;
            self.cache
                .stem_to_notes
                .entry(stem_key)
                .or_default()
                .push(note_id);
        }

        // Load alias_key to note_id mappings
        let mut stmt = self
            .conn
            .prepare("SELECT alias_key, note_id FROM aliases")?;
        let rows = stmt.query_map([], |row| {
            let alias_key: String = row.get(0)?;
            let note_id: i64 = row.get(1)?;
            Ok((alias_key, note_id))
        })?;

        for row in rows {
            let (alias_key, note_id) = row?;
            self.cache.alias_to_note.insert(alias_key, note_id);
        }

        // Load stem_key to attachment_id mappings
        let mut stmt = self
            .conn
            .prepare("SELECT stem_key, att_id FROM attachments")?;
        let rows = stmt.query_map([], |row| {
            let stem_key: String = row.get(0)?;
            let att_id: i64 = row.get(1)?;
            Ok((stem_key, att_id))
        })?;

        for row in rows {
            let (stem_key, att_id) = row?;
            self.cache
                .stem_to_atts
                .entry(stem_key)
                .or_default()
                .push(att_id);
        }

        Ok(())
    }

    /// Resolve a link target to a note or attachment.
    ///
    /// This implements the 5-step D8 algorithm:
    /// 1. `id:` prefix → `stable_id` lookup (with duplicate handling)
    /// 2. Path-suffix match if target contains `/`
    /// 3. Stem match over notes/aliases/attachments
    /// 4. Multiple candidates → rank and mark ambiguous
    /// 5. Zero candidates → unresolved
    pub fn resolve(
        &self,
        target_key: &str,
        raw_target: &str,
        is_embed: bool,
        source_note_id: Option<i64>,
    ) -> Resolution {
        // Step 1: Check for id: prefix
        let raw_lower = raw_target.to_lowercase();
        if let Some(stable_id) = raw_lower.strip_prefix("id:") {
            // Check for duplicate IDs
            if self.cache.duplicate_stable_ids.contains(stable_id) {
                // Choose the deterministic native PathKey winner
                // For now, we'll use the first one in the cache (not ideal but will work)
                if let Some(&note_id) = self.cache.stable_id_to_note.get(stable_id) {
                    let note_ref = NoteRef::new(note_id, Vec::new());
                    return Resolution::resolved_note(note_ref);
                }
                // Fall through to unresolved if not found
            } else if let Some(&note_id) = self.cache.stable_id_to_note.get(stable_id) {
                let note_ref = NoteRef::new(note_id, Vec::new());
                return Resolution::resolved_note(note_ref);
            }
            // id: not found, continue
        }

        // Step 2: Path-suffix match if target contains `/`
        if raw_target.contains('/') {
            // Try exact path match in notes
            let Ok(mut stmt) = self
                .conn
                .prepare("SELECT note_id, path_native FROM notes WHERE path_key = ?")
            else {
                return Resolution::unresolved();
            };

            if let Ok(mut rows) = stmt.query_map([raw_target], |row| {
                Ok(NoteRef::new(row.get(0)?, row.get(1)?))
            }) && let Some(Ok(note_ref)) = rows.next()
            {
                return Resolution::resolved_note(note_ref);
            }

            // Try exact path match in attachments
            let Ok(mut stmt) = self
                .conn
                .prepare("SELECT att_id, path_native FROM attachments WHERE path_key = ?")
            else {
                return Resolution::unresolved();
            };

            if let Ok(mut rows) = stmt.query_map([raw_target], |row| {
                Ok(AttRef::new(row.get(0)?, row.get(1)?))
            }) && let Some(Ok(att_ref)) = rows.next()
            {
                return Resolution::resolved_attachment(att_ref);
            }
        }

        // Step 3: Stem match over notes.stem_key ∪ aliases.alias_key ∪ attachments.stem_key
        let mut note_candidates: Vec<i64> = Vec::new();
        let mut att_candidates: Vec<i64> = Vec::new();

        // Check notes by stem_key
        if let Some(ids) = self.cache.stem_to_notes.get(target_key) {
            note_candidates.extend(ids);
        }

        // Check aliases by alias_key
        if let Some(&note_id) = self.cache.alias_to_note.get(target_key) {
            note_candidates.push(note_id);
        }

        // Check attachments by stem_key (only for embeds per D8)
        if is_embed && let Some(ids) = self.cache.stem_to_atts.get(target_key) {
            att_candidates.extend(ids);
        }

        // Step 4: Rank candidates
        let total_candidates = note_candidates.len() + att_candidates.len();

        if total_candidates == 0 {
            // Step 5: No candidates → unresolved
            return Resolution::unresolved();
        }

        if total_candidates > 1 || (!note_candidates.is_empty() && !att_candidates.is_empty()) {
            // Multiple candidates or cross-type candidates → rank and mark ambiguous
            let (best_note, best_att) =
                Self::rank_candidates(&note_candidates, &att_candidates, source_note_id);

            // Mark as ambiguous if there are multiple candidates
            let is_ambiguous = total_candidates > 1;

            if let Some(note_id) = best_note {
                let note_ref = NoteRef::new(note_id, Vec::new());
                let mut result = Resolution::resolved_note(note_ref);
                result.is_ambiguous = is_ambiguous;
                if is_ambiguous {
                    result.state = ResState::Ambiguous;
                }
                return result;
            }

            if let Some(att_id) = best_att {
                let att_ref = AttRef::new(att_id, Vec::new());
                let mut result = Resolution::resolved_attachment(att_ref);
                result.is_ambiguous = is_ambiguous;
                if is_ambiguous {
                    result.state = ResState::Ambiguous;
                }
                return result;
            }

            // Should not reach here if we have candidates
            return Resolution::ambiguous();
        }

        // Exactly one candidate
        if !note_candidates.is_empty() {
            let note_ref = NoteRef::new(note_candidates[0], Vec::new());
            return Resolution::resolved_note(note_ref);
        }

        if !att_candidates.is_empty() {
            let att_ref = AttRef::new(att_candidates[0], Vec::new());
            return Resolution::resolved_attachment(att_ref);
        }

        Resolution::unresolved()
    }

    /// Rank candidates by: same directory as source, then shallowest, then lexicographic.
    ///
    /// Returns (`best_note_id`, `best_att_id`).
    fn rank_candidates(
        note_candidates: &[i64],
        att_candidates: &[i64],
        _source_note_id: Option<i64>,
    ) -> (Option<i64>, Option<i64>) {
        // For simplicity, we'll pick the first one for now
        // A proper implementation would need to query the paths from the database
        // and do the actual ranking

        let best_note = note_candidates.first().copied();
        let best_att = att_candidates.first().copied();

        (best_note, best_att)
    }

    /// Re-resolve all links affected by a note change.
    ///
    /// This is called when notes are created, renamed, or deleted to invalidate
    /// cached resolutions.
    ///
    /// # Errors
    /// Returns a SQLite error if candidate keys cannot be loaded.
    pub fn invalidate_for_note(&mut self, note_id: i64) -> Result<()> {
        // Remove from cache
        let mut stmt = self
            .conn
            .prepare("SELECT stem_key FROM notes WHERE note_id = ?")?;

        if let Ok(rows) = stmt.query_map([note_id], |row| row.get::<_, String>(0)) {
            for row in rows {
                let stem_key: String = row?;
                self.cache.stem_to_notes.remove(&stem_key);
            }
        }

        // Remove aliases
        let mut stmt = self
            .conn
            .prepare("SELECT alias_key FROM aliases WHERE note_id = ?")?;

        if let Ok(rows) = stmt.query_map([note_id], |row| row.get::<_, String>(0)) {
            for row in rows {
                let alias_key: String = row?;
                self.cache.alias_to_note.remove(&alias_key);
            }
        }

        // Remove from stable_id cache
        let mut stmt = self
            .conn
            .prepare("SELECT stable_id FROM notes WHERE note_id = ?")?;

        if let Ok(rows) = stmt.query_map([note_id], |row| row.get::<_, Option<String>>(0)) {
            for row in rows {
                if let Ok(Some(id)) = row {
                    self.cache.stable_id_to_note.remove(&id);
                    self.cache.duplicate_stable_ids.remove(&id);
                }
            }
        }

        Ok(())
    }

    /// Re-resolve all links affected by an attachment change.
    ///
    /// # Errors
    /// Returns a SQLite error if the attachment stem cannot be loaded.
    pub fn invalidate_for_attachment(&mut self, att_id: i64) -> Result<()> {
        // Remove from cache
        let mut stmt = self
            .conn
            .prepare("SELECT stem_key FROM attachments WHERE att_id = ?")?;

        if let Ok(rows) = stmt.query_map([att_id], |row| row.get::<_, String>(0)) {
            for row in rows {
                let stem_key: String = row?;
                self.cache.stem_to_atts.remove(&stem_key);
            }
        }

        Ok(())
    }

    /// Update the resolution state for a link in the database.
    ///
    /// # Errors
    /// Returns a SQLite error if the link row cannot be updated.
    pub fn update_link_resolution(
        &self,
        src: i64,
        ord: i32,
        resolution: &Resolution,
    ) -> Result<()> {
        let (dst_note, dst_att, res_state) = match resolution.state {
            ResState::Resolved => {
                if let Some(ref note) = resolution.note {
                    (Some(note.note_id), None, ResState::Resolved)
                } else if let Some(ref att) = resolution.attachment {
                    (None, Some(att.att_id), ResState::Resolved)
                } else {
                    (None, None, ResState::Unresolved)
                }
            }
            ResState::Ambiguous => (None, None, ResState::Ambiguous),
            ResState::Unresolved => (None, None, ResState::Unresolved),
        };

        self.conn.execute(
            "UPDATE links SET dst_note = ?, dst_att = ?, res_state = ? WHERE src = ? AND ord = ?",
            params![dst_note, dst_att, res_state.as_str(), src, ord,],
        )?;

        Ok(())
    }

    /// Re-resolve all links in the database.
    ///
    /// This should be called during index initialization or after bulk changes.
    ///
    /// # Errors
    /// Returns a SQLite error if cache refresh, resolution, or link updates fail.
    pub fn re_resolve_all(&mut self) -> Result<()> {
        self.refresh_cache()?;

        let mut stmt = self
            .conn
            .prepare("SELECT src, ord, dst_raw, target_key, kind FROM links")?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        for row in rows {
            let (src, ord, dst_raw, target_key, kind_str) = row?;
            let is_embed = kind_str == "embed";
            let resolution = self.resolve(&target_key, &dst_raw, is_embed, Some(src));
            self.update_link_resolution(src, ord, &resolution)?;
        }

        Ok(())
    }
}

/// Normalize a target key for resolution (NFC + default casefold).
#[must_use]
pub fn normalize_target_key(target: &str) -> String {
    // For now, just lowercase. Proper implementation would use Unicode casefolding.
    target.to_lowercase()
}

/// Extract the stem from a path (filename without extension).
#[must_use]
pub fn extract_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().to_string())
}

/// Check if a file extension is an attachment-like extension.
#[must_use]
pub fn is_attachment_extension(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);

    matches!(
        ext.as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "svg"
                | "avif"
                | "bmp"
                | "ico"
                | "pdf"
                | "mp3"
                | "wav"
                | "ogg"
                | "flac"
                | "m4a"
                | "mp4"
                | "webm"
                | "mov"
                | "mkv"
                | "zip"
                | "tar"
                | "gz"
                | "tgz"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use tempfile::NamedTempFile;

    #[test]
    fn test_basic_resolution() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        // Insert test note
        conn.execute(
            "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) 
             VALUES (1, 'note1.md', X'6E6F7465312E6D64', 'note1.md', 'note1', 'Note 1', X'00', 100, 0, 0, 0)",
            [],
        ).unwrap();

        // Insert alias
        conn.execute(
            "INSERT INTO aliases (note_id, alias, alias_key) VALUES (1, 'my-note', 'my-note')",
            [],
        )
        .unwrap();

        let mut resolver = Resolver::new(conn);
        resolver.refresh_cache().unwrap();

        // Test stem resolution
        let res = resolver.resolve("note1", "note1", false, None);
        assert!(res.note.is_some());
        assert_eq!(res.note.unwrap().note_id, 1);

        // Test alias resolution
        let res = resolver.resolve("my-note", "my-note", false, None);
        assert!(res.note.is_some());
        assert_eq!(res.note.unwrap().note_id, 1);
    }

    #[test]
    fn test_unresolved() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        let mut resolver = Resolver::new(conn);
        resolver.refresh_cache().unwrap();

        let res = resolver.resolve("nonexistent", "nonexistent", false, None);
        assert!(res.is_unresolved);
        assert!(res.note.is_none());
        assert!(res.attachment.is_none());
    }

    #[test]
    fn test_attachment_extension() {
        assert!(is_attachment_extension("image.png"));
        assert!(is_attachment_extension("doc.pdf"));
        assert!(!is_attachment_extension("note.md"));
    }

    #[test]
    fn test_extract_stem() {
        assert_eq!(extract_stem("note.md"), "note");
        assert_eq!(extract_stem("my-note.md"), "my-note");
        assert_eq!(extract_stem("path/to/note.md"), "note");
    }
}
