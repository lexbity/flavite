//! Attachment rename handling (D8, §4.3, R3).
//!
//! This module handles rename detection and processing for attachments.
//! Attachments are first-class entities in Lattice, and their renames
//! need special handling.

use crate::channels::{Notice, NoticeChannel, RenameTier};
use flavite_index::types::Attachment;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Configuration for attachment rename detection.
#[derive(Debug, Clone)]
pub struct AttachmentRenameConfig {
    /// Whether to detect attachment renames via content hash (T3 for attachments).
    pub detect_by_content_hash: bool,
    /// Whether to detect attachment renames via fuzzy similarity (T4 for attachments).
    pub detect_by_similarity: bool,
}

impl Default for AttachmentRenameConfig {
    fn default() -> Self {
        Self {
            detect_by_content_hash: true,
            detect_by_similarity: true,
        }
    }
}

/// Information about a detected attachment rename.
#[derive(Debug, Clone, Default)]
pub struct AttachmentRename {
    /// The source path (vanished attachment).
    pub from: PathBuf,
    /// The destination path (new attachment).
    pub to: PathBuf,
    /// The inference tier.
    pub tier: RenameTier,
    /// Confidence score (0.0 to 1.0).
    pub confidence: f64,
    /// Number of embeds that reference this attachment.
    pub embed_count: u32,
}

/// Result of attachment rename detection.
#[derive(Debug, Clone, Default)]
pub struct AttachmentRenameResult {
    /// List of detected attachment renames.
    pub renames: Vec<AttachmentRename>,
    /// Set of attachment paths that were identified as T0 (in-app) renames.
    pub t0_renames: HashSet<PathBuf>,
}

/// Manager for attachment rename detection.
#[derive(Default)]
pub struct AttachmentRenameManager {
    config: AttachmentRenameConfig,
}

impl AttachmentRenameManager {
    /// Create a new attachment rename manager with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new attachment rename manager with custom configuration.
    #[must_use]
    pub fn with_config(config: AttachmentRenameConfig) -> Self {
        Self { config }
    }

    /// Detect attachment renames from vanished and changed attachment sets.
    ///
    /// This uses similar logic to note rename inference but adapted for
    /// attachments.
    ///
    /// # Arguments
    ///
    /// * `vanished` - Attachments that disappeared from disk
    /// * `changed` - Attachments that appeared or were modified on disk
    /// * `attachments_by_path` - Map from path to attachment
    /// * `embed_counts` - Map from attachment path to number of embeds referencing it
    /// * `notice_channel` - Channel to send rename notices to
    ///
    /// # Returns
    ///
    /// A result containing all detected attachment renames.
    pub fn detect_renames(
        &self,
        vanished: &[PathBuf],
        changed: &[PathBuf],
        attachments_by_path: &HashMap<PathBuf, &Attachment>,
        embed_counts: &HashMap<PathBuf, u32>,
        notice_channel: &NoticeChannel,
    ) -> AttachmentRenameResult {
        let mut result = AttachmentRenameResult::default();

        // For attachments, we primarily use content hash for inference (T3)
        // T1 (OS events) would be handled separately via watcher_pairs
        // T2 (stable_id) doesn't apply to attachments (they don't have stable_ids)

        // T3: Same content hash (if available)
        if self.config.detect_by_content_hash {
            self.detect_by_content_hash(
                vanished,
                changed,
                attachments_by_path,
                embed_counts,
                &mut result,
                notice_channel,
            );
        }

        // T4: Fuzzy similarity (if enabled)
        if self.config.detect_by_similarity {
            self.detect_by_similarity(
                vanished,
                changed,
                attachments_by_path,
                embed_counts,
                &mut result,
                notice_channel,
            );
        }

        result
    }

    /// Detect attachment renames by content hash matching.
    #[allow(clippy::collapsible_if)]
    fn detect_by_content_hash(
        &self,
        vanished: &[PathBuf],
        changed: &[PathBuf],
        attachments_by_path: &HashMap<PathBuf, &Attachment>,
        embed_counts: &HashMap<PathBuf, u32>,
        result: &mut AttachmentRenameResult,
        notice_channel: &NoticeChannel,
    ) {
        // Build map from content hash to paths for vanished attachments
        let mut hash_to_vanished: HashMap<Vec<u8>, Vec<PathBuf>> = HashMap::new();

        for path in vanished {
            if let Some(att) = attachments_by_path.get(path) {
                if let Some(hash) = &att.content_hash {
                    // Only consider if size is reasonable
                    if att.size >= 64 && att.size <= flavite_index::types::ATTACHMENT_HASH_MAX as i64
                    {
                        hash_to_vanished
                            .entry(hash.clone())
                            .or_default()
                            .push(path.clone());
                    }
                }
            }
        }

        // Check changed attachments
        for path in changed {
            if let Some(att) = attachments_by_path.get(path) {
                if let Some(hash) = &att.content_hash {
                    if att.size >= 64
                        && att.size <= flavite_index::types::ATTACHMENT_HASH_MAX as i64
                    {
                        if let Some(vanished_paths) = hash_to_vanished.get(hash) {
                            // Found matching hash
                            if vanished_paths.len() == 1 {
                                let from = &vanished_paths[0];
                                let embed_count = embed_counts.get(path).copied().unwrap_or(0);

                                let rename = AttachmentRename {
                                    from: from.clone(),
                                    to: path.clone(),
                                    tier: RenameTier::T3,
                                    confidence: 0.95,
                                    embed_count,
                                };

                                result.renames.push(rename.clone());

                                // Emit notice - never auto-rewrite for external renames
                                notice_channel.send(Notice::RenameSuggested {
                                    from: from.clone(),
                                    to: path.clone(),
                                    tier: RenameTier::T3,
                                    inbound: 0, // For attachments, we use embed_count
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    /// Detect attachment renames by fuzzy similarity.
    fn detect_by_similarity(
        &self,
        vanished: &[PathBuf],
        changed: &[PathBuf],
        attachments_by_path: &HashMap<PathBuf, &Attachment>,
        embed_counts: &HashMap<PathBuf, u32>,
        result: &mut AttachmentRenameResult,
        notice_channel: &NoticeChannel,
    ) {
        // For attachments, similarity is based on filename stems
        // This is a simplified implementation

        for from_path in vanished {
            if let Some(from_att) = attachments_by_path.get(from_path) {
                let from_stem = Self::path_to_stem(&from_att.path);

                for to_path in changed {
                    if let Some(to_att) = attachments_by_path.get(to_path) {
                        let to_stem = Self::path_to_stem(&to_att.path);

                        // Simple similarity: check if stems match
                        // In production, use proper string similarity
                        if from_stem == to_stem {
                            let embed_count = embed_counts.get(to_path).copied().unwrap_or(0);

                            let rename = AttachmentRename {
                                from: from_path.clone(),
                                to: to_path.clone(),
                                tier: RenameTier::T4,
                                confidence: 0.9, // Simple match is high confidence
                                embed_count,
                            };

                            result.renames.push(rename.clone());

                            // Emit notice
                            notice_channel.send(Notice::RenameSuggested {
                                from: from_path.clone(),
                                to: to_path.clone(),
                                tier: RenameTier::T4,
                                inbound: 0,
                            });

                            break; // Only one match per vanished attachment
                        }
                    }
                }
            }
        }
    }

    /// Register a T0 (in-app) attachment rename.
    pub fn register_t0_rename(&self, from: PathBuf, to: PathBuf, embed_count: u32) -> AttachmentRename {
        AttachmentRename {
            from,
            to,
            tier: RenameTier::T0,
            confidence: 1.0,
            embed_count,
        }
    }

    /// Extract stem from a path.
    fn path_to_stem(path: &str) -> String {
        use std::path::Path;

        Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_lowercase()
    }
}

/// Process in-app attachment renames.
///
/// For T0 renames, we can safely rewrite embeds in the journal.
/// For external renames, we emit a notice and let the user decide.
#[must_use]
pub fn should_rewrite_embeds(tier: RenameTier) -> bool {
    // Only T0 (in-app) renames auto-rewrite embeds
    matches!(tier, RenameTier::T0)
}

/// Get the embed count for an attachment path.
pub trait EmbedCountProvider {
    /// Get the number of embeds for a given attachment path.
    fn get_embed_count(&self, path: &std::path::Path) -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn mock_attachment(path: &str, content_hash: Option<Vec<u8>>, size: i64) -> Attachment {
        Attachment {
            att_id: 0,
            path: path.to_string(),
            path_native: path.as_bytes().to_vec(),
            path_key: path.to_lowercase(),
            stem_key: std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .to_lowercase(),
            kind: flavite_index::schema::AttachmentKind::Image,
            mime: Some("image/png".to_string()),
            size,
            content_hash,
            width: None,
            height: None,
            mtime_ns: 0,
            scan_ns: 0,
            hydrated: true,
            generation: 0,
        }
    }

    #[test]
    fn test_attachment_rename_by_content_hash() {
        let manager = AttachmentRenameManager::new();
        let notice_channel = NoticeChannel::new(std::time::Duration::from_secs(1));

        let hash = vec![1, 2, 3];
        let vanished_att = mock_attachment("old.png", Some(hash.clone()), 1000);
        let changed_att = mock_attachment("new.png", Some(hash), 1000);

        let mut attachments_by_path: HashMap<PathBuf, &Attachment> = HashMap::new();
        attachments_by_path.insert(PathBuf::from("old.png"), &vanished_att);
        attachments_by_path.insert(PathBuf::from("new.png"), &changed_att);

        let vanished = vec![PathBuf::from("old.png")];
        let changed = vec![PathBuf::from("new.png")];
        let embed_counts: HashMap<PathBuf, u32> = HashMap::new();

        let result = manager.detect_renames(
            &vanished,
            &changed,
            &attachments_by_path,
            &embed_counts,
            &notice_channel,
        );

        assert!(result.renames.len() > 0);
        let rename = &result.renames[0];
        assert_eq!(rename.from, PathBuf::from("old.png"));
        assert_eq!(rename.to, PathBuf::from("new.png"));
        assert_eq!(rename.tier, RenameTier::T3);
    }

    #[test]
    fn test_attachment_rename_by_similarity() {
        let manager = AttachmentRenameManager::new();
        let notice_channel = NoticeChannel::new(std::time::Duration::from_secs(1));

        // Same stem but different extensions
        let vanished_att = mock_attachment("diagram.png", None, 1000);
        let changed_att = mock_attachment("diagram.jpg", None, 1000);

        let mut attachments_by_path: HashMap<PathBuf, &Attachment> = HashMap::new();
        attachments_by_path.insert(PathBuf::from("diagram.png"), &vanished_att);
        attachments_by_path.insert(PathBuf::from("diagram.jpg"), &changed_att);

        let vanished = vec![PathBuf::from("diagram.png")];
        let changed = vec![PathBuf::from("diagram.jpg")];
        let embed_counts: HashMap<PathBuf, u32> = HashMap::new();

        let result = manager.detect_renames(
            &vanished,
            &changed,
            &attachments_by_path,
            &embed_counts,
            &notice_channel,
        );

        // Should detect based on stem similarity
        assert!(result.renames.len() > 0);
        let rename = &result.renames[0];
        assert_eq!(rename.tier, RenameTier::T4);
    }

    #[test]
    fn test_rewrite_embeds_rule() {
        assert!(should_rewrite_embeds(RenameTier::T0));
        assert!(!should_rewrite_embeds(RenameTier::T1));
        assert!(!should_rewrite_embeds(RenameTier::T2));
        assert!(!should_rewrite_embeds(RenameTier::T3));
        assert!(!should_rewrite_embeds(RenameTier::T4));
    }

    #[test]
    fn test_path_to_stem() {
        assert_eq!(
            AttachmentRenameManager::path_to_stem("My Diagram.png"),
            "my diagram"
        );
        assert_eq!(
            AttachmentRenameManager::path_to_stem("image.jpg"),
            "image"
        );
    }
}
