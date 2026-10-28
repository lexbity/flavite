//! Reconcile algorithm implementation (§4.4, D5).
//!
//! The reconcile algorithm maintains eventual consistency between the vault
//! filesystem (truth) and the index. It has two modes:
//!
//! - **FastMetadata**: uses stat (size + mtime) only, with the racy-file rule
//!   (mtime ≥ scan_time − 2s) to force hashing
//! - **VerifyContent**: hashes every eligible file in scope regardless of
//!   size/mtime equality
//!
//! Watcher-nominated paths always use VerifyContent. Periodic verification
//! (10 min) + focus reconciliation catch silent loss.

use crate::IndexPlaceholder;
use crate::channels::{Pulse, PulseChannel};
use crate::identity::ensure_flavite_dir;
use crate::types::{Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary, Version};
use flavite_fs::Fs;
use flavite_syntax::text::TextSnapshot;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Configuration for reconcile operations.
#[derive(Debug)]
pub struct ReconcileConfig {
    /// Maximum note size to parse/index (default: 32 MB).
    pub max_note_bytes: u64,
    /// Maximum attachment size to hash (default: 64 MB).
    pub max_attachment_bytes: u64,
    /// Verification interval (default: 10 minutes).
    pub verification_interval: Duration,
    /// Drift counter (for diagnostics).
    pub drift_found_total: AtomicU64,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            max_note_bytes: 32 * 1024 * 1024,                // 32 MB
            max_attachment_bytes: 64 * 1024 * 1024,          // 64 MB
            verification_interval: Duration::from_secs(600), // 10 minutes
            drift_found_total: AtomicU64::new(0),
        }
    }
}

/// State for tracking the reconcile process.
#[derive(Debug)]
pub struct ReconcileState {
    /// The last time a full verification completed.
    pub last_verification: Option<Instant>,
    /// The last time a reconcile was triggered by focus.
    pub last_focus_reconcile: Option<Instant>,
    /// Whether a verification is currently overdue.
    pub verification_overdue: bool,
    /// The current epoch.
    pub epoch: Epoch,
    /// The current generation.
    pub generation: Generation,
}

impl Default for ReconcileState {
    fn default() -> Self {
        Self {
            last_verification: None,
            last_focus_reconcile: None,
            verification_overdue: false,
            epoch: Epoch::new(0),
            generation: Generation::new(0),
        }
    }
}

/// Context for a reconcile operation.
pub struct ReconcileContext<'a> {
    /// The filesystem.
    pub fs: &'a dyn Fs,
    /// The index (placeholder for now).
    pub index: &'a mut IndexPlaceholder,
    /// The vault root.
    pub vault_root: &'a Path,
    /// The pulse channel.
    pub pulse: &'a PulseChannel,
    /// The configuration.
    pub config: &'a ReconcileConfig,
    /// The state.
    pub state: &'a mut ReconcileState,
}

/// Result of a reconcile operation.
#[derive(Debug, Clone)]
pub struct ReconcileResult {
    /// The summary of changes.
    pub summary: ReconcileSummary,
    /// The new version.
    pub version: Version,
    /// Whether a full verification is needed.
    pub verification_needed: bool,
}

/// Entry for tracking file state during reconcile.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub is_dir: bool,
    pub is_placeholder: bool,
}

/// Perform a reconcile operation.
///
/// This implements the algorithm from §4.4:
/// 1. Scan the filesystem for the scope
/// 2. Compare with the index
/// 3. Identify changed, vanished, and new files
/// 4. Apply changes to the index
/// 5. Send pulse notifications
pub fn reconcile(
    ctx: &mut ReconcileContext<'_>,
    scope: ReconcileScope,
    mode: ReconcileMode,
) -> ReconcileResult {
    let start_time = Instant::now();
    let scan_start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64;

    // Step 1: Scan the filesystem
    let disk_files = scan_disk(ctx.fs, ctx.vault_root, &scope);

    // Step 2: Get current index state
    let index_files = ctx.index.get_notes_in_scope(&scope);

    // Step 3: Identify changes
    let (changed, vanished, new_files) =
        identify_changes(ctx, &disk_files, &index_files, mode, scan_start);

    // Step 4: Apply changes to the index
    let mut summary = ReconcileSummary::default();

    for new_file in &new_files {
        if should_skip(&new_file.path) {
            continue;
        }
        if let Ok(snapshot) = read_and_parse(ctx.fs, &new_file.path, ctx.config)
            && ctx
                .index
                .apply_note(&new_file.path, &snapshot, scan_start)
                .is_ok()
        {
            summary.added += 1;
        }
    }

    for changed_file in &changed {
        if should_skip(&changed_file.path) {
            continue;
        }
        if let Ok(snapshot) = read_and_parse(ctx.fs, &changed_file.path, ctx.config)
            && ctx
                .index
                .update_note(&changed_file.path, &snapshot, scan_start)
                .is_ok()
        {
            summary.changed += 1;
        }
    }

    for vanished_file in &vanished {
        if should_skip(&vanished_file.path) {
            continue;
        }
        if ctx.index.remove_note(&vanished_file.path).is_ok() {
            summary.removed += 1;
        }
    }

    // Step 5: Update version and send pulse
    ctx.state.generation = ctx.state.generation.next();
    let version = Version::new(ctx.state.epoch, ctx.state.generation);

    ctx.pulse
        .send(Pulse::VaultChanged {
            epoch: ctx.state.epoch,
            generation: ctx.state.generation,
            summary,
        })
        .ok();

    // Check if verification is needed
    let verification_needed = check_verification_needed(ctx, &start_time);

    ReconcileResult {
        summary,
        version,
        verification_needed,
    }
}

/// Scan the filesystem for files in the given scope.
fn scan_disk(
    fs: &dyn Fs,
    vault_root: &Path,
    scope: &ReconcileScope,
) -> HashMap<PathBuf, FileEntry> {
    let mut files = HashMap::new();

    match scope {
        ReconcileScope::Single(path) => {
            if let Ok(metadata) = fs.metadata(path)
                && !metadata.is_dir
            {
                let entry = FileEntry {
                    path: path.to_path_buf(),
                    size: metadata.size,
                    mtime_ns: metadata.mtime_ns,
                    is_dir: false,
                    is_placeholder: fs.is_placeholder(path).unwrap_or(false),
                };
                files.insert(path.to_path_buf(), entry);
            }
        }
        ReconcileScope::Directory(dir) => {
            scan_dir_recursive(fs, dir, &mut files);
        }
        ReconcileScope::Full => {
            scan_dir_recursive(fs, vault_root, &mut files);
        }
    }

    files
}

/// Recursively scan a directory.
fn scan_dir_recursive(fs: &dyn Fs, dir: &Path, files: &mut HashMap<PathBuf, FileEntry>) {
    if let Ok(entries) = fs.read_dir(dir) {
        for entry in entries {
            let full_path = entry.path;

            // Skip .flavite directory and other ignored dirs
            if should_skip(&full_path) {
                continue;
            }

            if entry.is_dir {
                scan_dir_recursive(fs, &full_path, files);
            } else {
                if let Ok(metadata) = fs.metadata(&full_path) {
                    let entry = FileEntry {
                        path: full_path.clone(),
                        size: metadata.size,
                        mtime_ns: metadata.mtime_ns,
                        is_dir: false,
                        is_placeholder: fs.is_placeholder(&full_path).unwrap_or(false),
                    };
                    files.insert(full_path, entry);
                }
            }
        }
    }
}

/// Identify changed files between disk and index.
fn identify_changes(
    _ctx: &ReconcileContext<'_>,
    disk_files: &HashMap<PathBuf, FileEntry>,
    index_files: &HashMap<PathBuf, FileEntry>,
    mode: ReconcileMode,
    scan_time_ns: i64,
) -> (Vec<FileEntry>, Vec<FileEntry>, Vec<FileEntry>) {
    let mut changed = Vec::new();
    let mut vanished = Vec::new();
    let mut new_files = Vec::new();

    // Check for modified or new files on disk
    for (path, disk_entry) in disk_files {
        // Check if we're in VerifyContent mode or if this is a racy file
        let force_hash = mode == ReconcileMode::VerifyContent
            || disk_entry.mtime_ns >= scan_time_ns - 2_000_000_000; // 2 seconds in ns

        if let Some(index_entry) = index_files.get(path) {
            if force_hash
                || disk_entry.size != index_entry.size
                || disk_entry.mtime_ns != index_entry.mtime_ns
            {
                // Need to hash and compare
                changed.push(disk_entry.clone());
            }
        } else {
            // New file
            new_files.push(disk_entry.clone());
        }
    }

    // Check for files that exist in index but not on disk
    for (path, index_entry) in index_files {
        if !disk_files.contains_key(path) {
            vanished.push(index_entry.clone());
        }
    }

    (changed, vanished, new_files)
}

/// Check if a path should be skipped during reconcile.
fn should_skip(path: &Path) -> bool {
    let path_str = path.display().to_string();

    // Skip .flavite directory
    if path_str.contains("/.flavite/") || path_str.ends_with("/.flavite") {
        return true;
    }

    // Skip .obsidian directory
    if path_str.contains("/.obsidian/") || path_str.ends_with("/.obsidian") {
        return true;
    }

    // Skip other dotfiles/directories at vault root
    let components: Vec<&str> = path_str.split('/').collect();
    if !components.is_empty() {
        let last = components.last().unwrap();
        if last.starts_with('.') && !last.starts_with(".flavite") && !last.starts_with(".obsidian")
        {
            return true;
        }
    }

    false
}

/// Read and parse a file.
fn read_and_parse(
    fs: &dyn Fs,
    path: &Path,
    config: &ReconcileConfig,
) -> Result<TextSnapshot, String> {
    // Check if it's a placeholder
    if fs.is_placeholder(path).unwrap_or(false) {
        return Err("Cloud placeholder".to_string());
    }

    // Read with size cap
    match fs.read_capped(path, config.max_note_bytes) {
        Ok(bytes) => TextSnapshot::new(&bytes).map_err(|e| format!("Parse error: {e}")),
        Err(e) => Err(format!("Read error: {e}")),
    }
}

/// Check if verification is needed based on time.
fn check_verification_needed(ctx: &mut ReconcileContext<'_>, _start_time: &Instant) -> bool {
    let now = Instant::now();

    // Check if verification interval has passed
    if let Some(last) = ctx.state.last_verification {
        if now.duration_since(last) >= ctx.config.verification_interval {
            ctx.state.verification_overdue = true;
            return true;
        }
    } else {
        // Never verified
        ctx.state.verification_overdue = true;
        return true;
    }

    // Check if this was a focus-triggered reconcile
    if let Some(_last_focus) = ctx.state.last_focus_reconcile {
        // Focus-triggered reconciles should still check the interval
        // but we don't force verification on every focus
    }

    ctx.state.verification_overdue = false;
    false
}

/// Perform a full verification reconcile.
pub fn verify_content_full(ctx: &mut ReconcileContext<'_>) -> ReconcileResult {
    reconcile(ctx, ReconcileScope::Full, ReconcileMode::VerifyContent)
}

/// Perform a fast metadata reconcile.
pub fn fast_metadata_full(ctx: &mut ReconcileContext<'_>) -> ReconcileResult {
    reconcile(ctx, ReconcileScope::Full, ReconcileMode::FastMetadata)
}

/// Initialize a new vault by performing an initial scan.
pub fn initial_scan(
    fs: &dyn Fs,
    vault_root: &Path,
    index: &mut IndexPlaceholder,
    pulse: &PulseChannel,
    config: &ReconcileConfig,
) -> ReconcileResult {
    let mut state = ReconcileState::default();
    let mut ctx = ReconcileContext {
        fs,
        index,
        vault_root,
        pulse,
        config,
        state: &mut state,
    };

    // Ensure .flavite directory exists
    let _ = ensure_flavite_dir(vault_root);

    // Initial scan uses VerifyContent mode
    let result = reconcile(&mut ctx, ReconcileScope::Full, ReconcileMode::VerifyContent);

    // Mark verification as completed
    ctx.state.last_verification = Some(Instant::now());

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_fs::MemFs;
    use std::path::Path;

    #[test]
    fn test_should_skip() {
        assert!(should_skip(Path::new("/vault/.flavite/vault.toml")));
        assert!(should_skip(Path::new("/vault/.obsidian/plugins")));
        assert!(should_skip(Path::new("/vault/.git")));
        assert!(!should_skip(Path::new("/vault/notes/test.md")));
        assert!(!should_skip(Path::new("/vault/attachments/image.png")));
    }

    #[test]
    fn test_scan_empty() {
        let fs = MemFs::new();
        let vault_root = Path::new("/vault");

        // Create vault directory
        fs.create_dir_all(vault_root).unwrap();

        let files = scan_disk(&fs, vault_root, &ReconcileScope::Full);
        assert!(files.is_empty());
    }

    #[test]
    fn test_scan_with_files() {
        let fs = MemFs::new();
        let vault_root = Path::new("/vault");

        // Create vault directory and some files
        fs.create_dir_all(vault_root).unwrap();
        fs.write_new(&vault_root.join("note1.md"), b"# Note 1")
            .unwrap();
        fs.write_new(&vault_root.join("note2.md"), b"# Note 2")
            .unwrap();

        let files = scan_disk(&fs, vault_root, &ReconcileScope::Full);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_identify_changes_new_file() {
        let fs = MemFs::new();
        let vault_root = Path::new("/vault");

        fs.create_dir_all(vault_root).unwrap();
        fs.write_new(&vault_root.join("new.md"), b"new content")
            .unwrap();

        let disk_files = scan_disk(&fs, vault_root, &ReconcileScope::Full);
        let index_files = HashMap::new();

        let mut index = IndexPlaceholder;
        let (changed, vanished, new_files) = identify_changes(
            &ReconcileContext {
                fs: &fs,
                index: &mut index,
                vault_root,
                pulse: &PulseChannel::new(1024),
                config: &ReconcileConfig::default(),
                state: &mut ReconcileState::default(),
            },
            &disk_files,
            &index_files,
            ReconcileMode::VerifyContent,
            0,
        );

        assert_eq!(new_files.len(), 1);
        assert!(changed.is_empty());
        assert!(vanished.is_empty());
    }
}
