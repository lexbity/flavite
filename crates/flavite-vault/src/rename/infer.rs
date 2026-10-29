//! Tiered rename inference engine (D8, §4.8, R6).
//!
//! This module implements the tiered rename inference algorithm that detects
//! file renames and emits appropriate notices. The tiers decide not only link
//! repair but **which side effects are silent**:
//!
//! | Tier | Evidence | Buffer re-key | Pin carry-over | Link repair |
//! |------|----------|---------------|---------------|-------------|
//! | **T0** | In-app rename command | yes, silent | yes, silent | journaled rewrite per setting (D9) |
//! | **T1** | OS rename event pair | yes, silent | yes, silent | notice; never auto-rewrite (external) |
//! | **T2** | Same `stable_id` on a vanished + created pair | yes, silent | yes, silent | notice; never auto-rewrite (external) |
//! | **T3** | Same content hash, size ≥ 64 B, exactly one candidate | **no** | **no** | notice only; user confirms |
//! | **T4** | Fuzzy similarity ≥ 0.9, exactly one candidate | **no** | **no** | notice only; user confirms |
//!
//! Otherwise it's delete + create, and links go unresolved and are reported
//! rather than guessed.

use crate::channels::RenameTier;
use flavite_index::types::{MAX_NOTE_BYTES, Note};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Configuration for the rename inference engine.
#[derive(Debug, Clone)]
pub struct RenameInferenceConfig {
    /// Minimum file size in bytes for T3 inference (default: 64).
    pub t3_min_size: usize,
    /// Minimum similarity score for T4 inference (default: 0.9).
    pub t4_min_similarity: f64,
    /// Whether T3 inference is enabled (default: true).
    pub t3_enabled: bool,
    /// Whether T4 inference is enabled (default: true).
    pub t4_enabled: bool,
}

impl Default for RenameInferenceConfig {
    fn default() -> Self {
        Self {
            t3_min_size: 64,
            t4_min_similarity: 0.9,
            t3_enabled: true,
            t4_enabled: true,
        }
    }
}

/// A detected rename candidate.
#[derive(Debug, Clone, Default)]
pub struct RenameCandidate {
    /// The source path (vanished file).
    pub from: PathBuf,
    /// The destination path (new file).
    pub to: PathBuf,
    /// The inference tier.
    pub tier: RenameTier,
    /// Confidence score (0.0 to 1.0).
    pub confidence: f64,
    /// Number of inbound links that would be affected.
    pub inbound_links: u32,
}

/// Result of rename inference.
#[derive(Debug, Clone, Default)]
pub struct RenameInferenceResult {
    /// List of detected rename candidates.
    pub candidates: Vec<RenameCandidate>,
    /// Set of paths that were identified as T2 (stable_id) renames.
    pub t2_renames: HashSet<(PathBuf, PathBuf)>,
}

/// The rename inference engine.
#[derive(Default)]
pub struct RenameInference {
    /// Configuration.
    config: RenameInferenceConfig,
}

impl RenameInference {
    /// Create a new rename inference engine with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new rename inference engine with custom configuration.
    #[must_use]
    pub fn with_config(config: RenameInferenceConfig) -> Self {
        Self { config }
    }

    /// Infer rename pairs from one reconcile's vanished and newly-created
    /// note sets. `old_notes` is snapshotted before removals and `new_notes`
    /// comes from the index after ingest, so vanished rows remain available.
    pub fn infer_reconcile(
        &self,
        vanished: &[PathBuf],
        created: &[PathBuf],
        old_notes: &HashMap<PathBuf, Note>,
        new_notes: &HashMap<PathBuf, Note>,
        inbound_link_counts: &HashMap<PathBuf, u32>,
    ) -> RenameInferenceResult {
        let mut result = RenameInferenceResult::default();
        if vanished.is_empty() || created.is_empty() {
            return result;
        }

        let paths_by_id = |notes: &HashMap<PathBuf, Note>| {
            let mut grouped = HashMap::<String, Vec<PathBuf>>::new();
            for (path, note) in notes {
                if let Some(id) = note.stable_id.as_deref() {
                    grouped
                        .entry(flavite_syntax::text::fold_key(id))
                        .or_default()
                        .push(path.clone());
                }
            }
            grouped
        };
        // Uniqueness is a property of the complete index snapshot. Looking
        // only at the changed paths would mistake one member of a duplicate
        // stable-id group for an unambiguous rename.
        let old_by_id = paths_by_id(old_notes);
        let new_by_id = paths_by_id(new_notes);
        let mut claimed_old = HashSet::new();
        let mut claimed_new = HashSet::new();
        for (id, old_paths) in &old_by_id {
            let Some(new_paths) = new_by_id.get(id) else {
                continue;
            };
            if old_paths.len() != 1 || new_paths.len() != 1 {
                continue;
            }
            let from = old_paths[0].clone();
            let to = new_paths[0].clone();
            if !vanished.contains(&from)
                || !created.contains(&to)
                || old_notes.get(&from).is_none_or(|note| note.id_conflict)
                || new_notes.get(&to).is_none_or(|note| note.id_conflict)
            {
                continue;
            }
            let inbound = inbound_link_counts.get(&from).copied().unwrap_or(0);
            claimed_old.insert(from.clone());
            claimed_new.insert(to.clone());
            result.t2_renames.insert((from.clone(), to.clone()));
            result.candidates.push(RenameCandidate {
                from,
                to,
                tier: RenameTier::T2,
                confidence: 1.0,
                inbound_links: inbound,
            });
        }

        if self.config.t3_enabled {
            let group_hashes = |notes: &HashMap<PathBuf, Note>| {
                let mut grouped = HashMap::<Vec<u8>, Vec<PathBuf>>::new();
                for (path, note) in notes {
                    if usize::try_from(note.size)
                        .is_ok_and(|size| size >= self.config.t3_min_size && size <= MAX_NOTE_BYTES)
                        && note.content_hash.len() == 32
                    {
                        grouped
                            .entry(note.content_hash.clone())
                            .or_default()
                            .push(path.clone());
                    }
                }
                grouped
            };
            // As with IDs, T3 is only evidence when the content is unique in
            // each full snapshot, not merely unique among this reconcile's
            // changed paths.
            let old_hashes = group_hashes(old_notes);
            let new_hashes = group_hashes(new_notes);
            for (hash, old_paths) in old_hashes {
                let Some(new_paths) = new_hashes.get(&hash) else {
                    continue;
                };
                if old_paths.len() != 1 || new_paths.len() != 1 {
                    continue;
                }
                let from = old_paths[0].clone();
                let to = new_paths[0].clone();
                if claimed_old.contains(&from)
                    || claimed_new.contains(&to)
                    || !vanished.contains(&from)
                    || !created.contains(&to)
                {
                    continue;
                }
                let inbound = inbound_link_counts.get(&from).copied().unwrap_or(0);
                claimed_old.insert(from.clone());
                claimed_new.insert(to.clone());
                result.candidates.push(RenameCandidate {
                    from,
                    to,
                    tier: RenameTier::T3,
                    confidence: 1.0,
                    inbound_links: inbound,
                });
            }
        }

        if self.config.t4_enabled {
            let old_remaining = vanished
                .iter()
                .filter(|path| !claimed_old.contains(*path) && old_notes.contains_key(*path))
                .collect::<Vec<_>>();
            let new_remaining = created
                .iter()
                .filter(|path| !claimed_new.contains(*path) && new_notes.contains_key(*path))
                .collect::<Vec<_>>();
            let mut old_matches = HashMap::<PathBuf, Vec<(PathBuf, f64)>>::new();
            let mut new_match_counts = HashMap::<PathBuf, usize>::new();
            for from in &old_remaining {
                let old_stem = Self::path_to_stem(&old_notes[*from].path);
                for to in &new_remaining {
                    let new_stem = Self::path_to_stem(&new_notes[*to].path);
                    let score = Self::compute_similarity(&old_stem, &new_stem);
                    if score >= self.config.t4_min_similarity {
                        old_matches
                            .entry((*from).clone())
                            .or_default()
                            .push(((*to).clone(), score));
                        *new_match_counts.entry((*to).clone()).or_default() += 1;
                    }
                }
            }
            for (from, matches) in old_matches {
                if matches.len() != 1 {
                    continue;
                }
                let (to, confidence) = &matches[0];
                if new_match_counts.get(to) != Some(&1) {
                    continue;
                }
                result.candidates.push(RenameCandidate {
                    inbound_links: inbound_link_counts.get(&from).copied().unwrap_or(0),
                    from,
                    to: to.clone(),
                    tier: RenameTier::T4,
                    confidence: *confidence,
                });
            }
        }
        result
    }

    /// Register a T0 (in-app) rename.
    ///
    /// T0 renames are silent for buffer re-key and pin carry-over,
    /// and use journaled rewrite per the setting (D9).
    pub fn register_t0_rename(
        &self,
        from: PathBuf,
        to: PathBuf,
        inbound_links: u32,
        tier: RenameTier,
    ) -> RenameCandidate {
        RenameCandidate {
            from,
            to,
            tier,
            confidence: 1.0,
            inbound_links,
        }
    }

    /// Extract stem from a path.
    fn path_to_stem(path: &str) -> String {
        let path_obj = Path::new(path);
        let stem = path_obj
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path);
        flavite_syntax::text::fold_key(stem)
    }

    /// Compute normalized character-level edit similarity.
    fn compute_similarity(a: &str, b: &str) -> f64 {
        if a.is_empty() || b.is_empty() {
            return 0.0;
        }
        let a = flavite_syntax::text::fold_key(a);
        let b = flavite_syntax::text::fold_key(b);
        similar::TextDiff::from_chars(&a, &b).ratio() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Mock note for testing
    fn mock_note(path: &str, stable_id: Option<&str>, content_hash: Vec<u8>, size: i64) -> Note {
        Note {
            note_id: 0,
            path: path.to_string(),
            path_native: path.as_bytes().to_vec(),
            path_key: path.to_lowercase(),
            stem_key: Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .to_lowercase(),
            title: path.to_string(),
            stable_id: stable_id.map(|s| s.to_string()),
            id_conflict: false,
            content_hash,
            size,
            mtime_ns: 0,
            scan_ns: 0,
            frontmatter: None,
            preview: None,
            parse_ok: true,
            generation: 0,
        }
    }

    #[test]
    fn reconcile_inference_uses_unique_t2_t3_t4_evidence() {
        let mut old_notes = HashMap::new();
        let mut new_notes = HashMap::new();
        let t2_old = mock_note("notes/old-id.md", Some("Été"), vec![1; 32], 80);
        let t2_new = mock_note("notes/new-id.md", Some("E\u{301}TÉ"), vec![2; 32], 80);
        let t3_old = mock_note("notes/old-copy.md", None, vec![3; 32], 80);
        let t3_new = mock_note("archive/new-copy.md", None, vec![3; 32], 80);
        let t4_old = mock_note("notes/proposal.md", None, vec![4; 32], 80);
        let t4_new = mock_note("notes/proposals.md", None, vec![5; 32], 80);
        let ambiguous_old = mock_note("notes/report.md", None, vec![6; 32], 80);
        let ambiguous_a = mock_note("notes/reports.md", None, vec![7; 32], 80);
        let ambiguous_b = mock_note("notes/reportx.md", None, vec![8; 32], 80);
        for note in [t2_old, t3_old, t4_old, ambiguous_old] {
            old_notes.insert(PathBuf::from(&note.path), note);
        }
        for note in [t2_new, t3_new, t4_new, ambiguous_a, ambiguous_b] {
            new_notes.insert(PathBuf::from(&note.path), note);
        }
        let vanished = old_notes.keys().cloned().collect::<Vec<_>>();
        let created = new_notes.keys().cloned().collect::<Vec<_>>();
        let inbound = HashMap::from([(PathBuf::from("notes/old-id.md"), 4)]);
        let result = RenameInference::new()
            .infer_reconcile(&vanished, &created, &old_notes, &new_notes, &inbound);

        assert!(result.t2_renames.contains(&(
            PathBuf::from("notes/old-id.md"),
            PathBuf::from("notes/new-id.md")
        )));
        assert_eq!(
            result
                .candidates
                .iter()
                .find(|candidate| candidate.tier == RenameTier::T2)
                .unwrap()
                .inbound_links,
            4
        );
        assert!(result.candidates.iter().any(|candidate| {
            candidate.tier == RenameTier::T3
                && candidate.from.as_path() == Path::new("notes/old-copy.md")
                && candidate.to.as_path() == Path::new("archive/new-copy.md")
        }));
        assert!(result.candidates.iter().any(|candidate| {
            candidate.tier == RenameTier::T4
                && candidate.from.as_path() == Path::new("notes/proposal.md")
                && candidate.to.as_path() == Path::new("notes/proposals.md")
        }));
        assert!(
            !result
                .candidates
                .iter()
                .any(|candidate| candidate.from.as_path() == Path::new("notes/report.md"))
        );

        let empty =
            RenameInference::new().infer_reconcile(&[], &created, &old_notes, &new_notes, &inbound);
        assert!(empty.candidates.is_empty());
        assert!(empty.t2_renames.is_empty());
    }

    #[test]
    fn reconcile_t2_and_t3_require_global_uniqueness() {
        let duplicate_id = flavite_syntax::text::fold_key("Copied-ID");
        let mut old_notes = HashMap::new();
        let mut new_notes = HashMap::new();
        let old_id_path = PathBuf::from("notes/old-id.md");
        let keeper_id_path = PathBuf::from("notes/keeper-id.md");
        let new_id_path = PathBuf::from("notes/new-id.md");
        let old_hash_path = PathBuf::from("notes/old-copy.md");
        let new_hash_path = PathBuf::from("notes/new-copy.md");
        let mut old_loser = mock_note("notes/old-id.md", Some(&duplicate_id), vec![1; 32], 80);
        old_loser.id_conflict = false;
        let mut old_keeper = mock_note("notes/keeper-id.md", Some(&duplicate_id), vec![2; 32], 80);
        old_keeper.id_conflict = true;
        let mut new_keeper = mock_note("notes/keeper-id.md", Some(&duplicate_id), vec![2; 32], 80);
        new_keeper.id_conflict = true;
        let mut new_loser = mock_note("notes/new-id.md", Some(&duplicate_id), vec![3; 32], 80);
        new_loser.id_conflict = true;
        for note in [
            old_loser,
            old_keeper,
            mock_note("notes/old-copy.md", None, vec![9; 32], 80),
            mock_note("notes/keeper-copy.md", None, vec![9; 32], 80),
        ] {
            old_notes.insert(PathBuf::from(&note.path), note);
        }
        for note in [
            new_keeper,
            new_loser,
            mock_note("notes/keeper-copy.md", None, vec![9; 32], 80),
            mock_note("notes/new-copy.md", None, vec![9; 32], 80),
        ] {
            new_notes.insert(PathBuf::from(&note.path), note);
        }

        let result = RenameInference::new().infer_reconcile(
            &[old_id_path.clone(), old_hash_path.clone()],
            &[new_id_path, new_hash_path],
            &old_notes,
            &new_notes,
            &HashMap::new(),
        );
        assert!(
            !result
                .t2_renames
                .contains(&(old_id_path.clone(), PathBuf::from("notes/new-id.md")))
        );
        assert!(!result.candidates.iter().any(|candidate| {
            candidate.tier == RenameTier::T2 && candidate.from == old_id_path
        }));
        assert!(!result.candidates.iter().any(|candidate| {
            candidate.tier == RenameTier::T3 && candidate.from == old_hash_path
        }));
        assert_eq!(old_notes.len(), 4);
        assert_eq!(new_notes.len(), 4);
        assert!(old_notes.contains_key(&keeper_id_path));
    }

    #[test]
    fn test_path_to_stem() {
        assert_eq!(RenameInference::path_to_stem("My Note.md"), "my note");
        assert_eq!(RenameInference::path_to_stem("note.txt"), "note");
        assert_eq!(RenameInference::path_to_stem("just-name"), "just-name");
    }

    #[test]
    fn test_compute_similarity() {
        // Test identical strings
        let sim = RenameInference::compute_similarity("hello", "hello");
        assert_eq!(sim, 1.0); // Should be 1.0 for identical

        // Test completely different
        let sim = RenameInference::compute_similarity("hello", "world");
        // With our simple char-based similarity, this will have some overlap
        // (both have 'l' and 'o'), so we just check it's <= 1.0
        assert!((0.0..=1.0).contains(&sim));

        // Test partial match
        let sim = RenameInference::compute_similarity("hello world", "hello");
        // Should be > 0.0 for partial match
        assert!(sim > 0.0 && sim <= 1.0);

        // Test empty strings
        let sim = RenameInference::compute_similarity("", "hello");
        assert_eq!(sim, 0.0);
        let sim = RenameInference::compute_similarity("", "");
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn test_rename_tier_display() {
        assert_eq!(RenameTier::T0.as_str(), "T0");
        assert_eq!(RenameTier::T1.as_str(), "T1");
        assert_eq!(RenameTier::T2.as_str(), "T2");
        assert_eq!(RenameTier::T3.as_str(), "T3");
        assert_eq!(RenameTier::T4.as_str(), "T4");
    }

    #[test]
    fn test_t3_min_size_config() {
        let config = RenameInferenceConfig::default();
        assert_eq!(config.t3_min_size, 64);
        assert_eq!(config.t4_min_similarity, 0.9);
    }
}
