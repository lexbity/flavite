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
//! Duplicate IDs are co-ingested. The lowest folded path key wins `id:` lookup;
//! remaining rows carry `id_conflict` and are reported by doctor.

use crate::schema::ResState;
use crate::types::{AttRef, NoteRef};
use rusqlite::{Connection, Result, params};
use std::path::Path;

/// Pure candidate ordering used by persisted and on-demand resolution.
/// Candidate collection remains index-owned; this module owns the ranking
/// rule itself.
pub mod ranking {
    /// Candidate attributes used by the deterministic resolver ranking.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Candidate {
        pub same_directory: bool,
        pub depth: usize,
        pub path: String,
        pub is_note: bool,
        pub id: i64,
    }

    /// Sort same-directory candidates first, then shallowest path, then
    /// lexicographic folded display path.
    pub fn rank_candidates(candidates: &mut [Candidate]) {
        candidates.sort_by(|a, b| {
            b.same_directory
                .cmp(&a.same_directory)
                .then(a.depth.cmp(&b.depth))
                .then(a.path.cmp(&b.path))
        });
    }
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
pub struct Resolver<'a> {
    conn: &'a Connection,
    catalog: Option<crate::catalog::ResolverCatalog>,
}

impl<'a> Resolver<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self {
            conn,
            catalog: None,
        }
    }

    /// Initialize or refresh the cache from the database.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the backing queries cannot be prepared
    /// or their rows cannot be read.
    pub fn refresh_cache(&mut self) -> Result<()> {
        self.catalog = Some(crate::catalog::ResolverCatalog::load(self.conn)?);
        Ok(())
    }

    /// Resolve a new unsaved source with the same directory-ranking rule.
    #[must_use]
    pub fn resolve_at_path(
        &self,
        target_key: &str,
        raw: &str,
        embed: bool,
        source: &Path,
    ) -> Resolution {
        self.catalog
            .as_ref()
            .map_or_else(Resolution::unresolved, |catalog| {
                catalog.resolve_at_path(target_key, raw, embed, Some(source))
            })
    }

    /// Resolve a link target using the canonical D8 precedence and ranking.
    #[must_use]
    pub fn resolve(
        &self,
        target_key: &str,
        raw_target: &str,
        is_embed: bool,
        source_note_id: Option<i64>,
    ) -> Resolution {
        self.catalog
            .as_ref()
            .map_or_else(Resolution::unresolved, |catalog| {
                catalog.resolve(target_key, raw_target, is_embed, source_note_id)
            })
    }

    /// Re-resolve all links affected by a note change.
    ///
    /// This is called when notes are created, renamed, or deleted to invalidate
    /// cached resolutions.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the cache-refresh queries cannot be
    /// prepared; unreadable rows are skipped.
    pub fn invalidate_for_note(&mut self, note_id: i64) -> Result<()> {
        if let Some(catalog) = &mut self.catalog {
            catalog.invalidate_note(note_id);
        }
        Ok(())
    }

    /// Invalidate canonical attachment candidates after an index change.
    ///
    /// # Errors
    /// Retains the existing fallible API; the immutable in-memory removal cannot fail.
    pub fn invalidate_for_attachment(&mut self, att_id: i64) -> Result<()> {
        if let Some(catalog) = &mut self.catalog {
            catalog.invalidate_attachment(att_id);
        }
        Ok(())
    }

    /// Update the resolution state for a link in the database.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the update cannot be executed.
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
            ResState::Ambiguous => (
                resolution.note.as_ref().map(|note| note.note_id),
                resolution
                    .attachment
                    .as_ref()
                    .map(|attachment| attachment.att_id),
                ResState::Ambiguous,
            ),
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
    ///
    /// Returns a SQLite error when the cache refresh or any link re-resolution
    /// query fails.
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

        let mut resolver = Resolver::new(&conn);
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

        let mut resolver = Resolver::new(&conn);
        resolver.refresh_cache().unwrap();

        let res = resolver.resolve("nonexistent", "nonexistent", false, None);
        assert!(res.is_unresolved);
        assert!(res.note.is_none());
        assert!(res.attachment.is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn resolver_handles_paths_ids_embeds_ambiguity_and_invalidation() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        for (id, path, native, stem, stable) in [
            (
                1,
                "notes/one.md",
                b"notes/one.md".as_slice(),
                "one",
                "shared",
            ),
            (
                2,
                "notes/two.md",
                b"notes/two.md".as_slice(),
                "two",
                "shared",
            ),
            (
                3,
                "notes/cover.md",
                b"notes/cover.md".as_slice(),
                "cover",
                "cover-id",
            ),
        ] {
            conn.execute(
                "INSERT INTO notes (note_id,path,path_native,path_key,stem_key,title,stable_id,stable_id_key,content_hash,size,mtime_ns,scan_ns,generation) VALUES (?1,?2,?3,?2,?4,'',?5,?5,X'00',0,0,0,0)",
                params![id, path, native, stem, stable],
            ).unwrap();
        }
        conn.execute(
            "INSERT INTO aliases (note_id,alias,alias_key) VALUES (1,'first','alias')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachments (att_id,path,path_native,path_key,stem_key,kind,size,mtime_ns,scan_ns,generation) VALUES (5,'images/cover.png',X'696d61676573','images/cover.png','cover','image',1,0,0,0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO links (src,ord,kind,dst_raw,target_key,by_id,span_start,span_end,res_state) VALUES (1,0,'link','two','two',0,0,0,'unresolved')",
            [],
        ).unwrap();

        let mut resolver = Resolver::new(&conn);
        resolver.refresh_cache().unwrap();
        assert_eq!(
            resolver
                .resolve("cover-id", "id:cover-id", false, None)
                .note
                .unwrap()
                .note_id,
            3
        );
        let duplicate_id = resolver.resolve("shared", "id:shared", false, None);
        assert_eq!(duplicate_id.note.unwrap().note_id, 1);
        assert_eq!(duplicate_id.state, ResState::Resolved);
        assert!(!duplicate_id.is_ambiguous);
        assert_eq!(
            resolver
                .resolve("notes/one.md", "notes/one.md", false, None)
                .note
                .unwrap()
                .note_id,
            1
        );
        assert_eq!(
            resolver
                .resolve("images/cover.png", "images/cover.png", true, None)
                .attachment
                .unwrap()
                .att_id,
            5
        );
        assert_eq!(
            resolver
                .resolve("alias", "first", false, None)
                .note
                .unwrap()
                .note_id,
            1
        );
        let ambiguous = resolver.resolve("cover", "cover", true, None);
        assert!(!ambiguous.is_ambiguous);
        assert_eq!(ambiguous.attachment.unwrap().att_id, 5);

        resolver.re_resolve_all().unwrap();
        let state: String = resolver
            .conn
            .query_row(
                "SELECT res_state FROM links WHERE src=1 AND ord=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "resolved");
        resolver.invalidate_for_note(1).unwrap();
        resolver.invalidate_for_attachment(5).unwrap();
        assert!(
            resolver
                .resolve("alias", "first", false, None)
                .is_unresolved
        );
        resolver.refresh_cache().unwrap();
        let resolution = Resolution::ambiguous();
        resolver.update_link_resolution(1, 0, &resolution).unwrap();
        let state: String = resolver
            .conn
            .query_row(
                "SELECT res_state FROM links WHERE src=1 AND ord=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "ambiguous");
        resolver
            .update_link_resolution(1, 0, &Resolution::unresolved())
            .unwrap();
        resolver
            .update_link_resolution(
                1,
                0,
                &Resolution {
                    note: None,
                    attachment: None,
                    is_ambiguous: false,
                    is_unresolved: false,
                    state: ResState::Resolved,
                },
            )
            .unwrap();
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
