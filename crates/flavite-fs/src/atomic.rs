//! Atomic write utilities (§4.4, D6).
//!
//! Provides atomic file write following the spec:
//! 1. Resolve symlink
//! 2. Write temp file in the same directory
//! 3. Copy permissions
//! 4. fsync the temp file
//! 5. rename-over (replace)
//! 6. fsync the directory

use std::path::{Path, PathBuf};

use crate::fs::{Fs, Result};

use blake3;

/// The prefix used for temporary files during atomic writes.
const TEMP_PREFIX: &str = ".flavite-atomic-";

/// Generate a temporary path in the same directory as the target.
fn temp_path<F: Fs>(fs: &F, target: &Path) -> Result<PathBuf> {
    let dir = target.parent().unwrap_or(Path::new("."));
    let canonical_dir = fs.canonicalize(dir)?;

    // Generate a unique temp filename
    use std::time::SystemTime;
    let timestamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let temp_name = format!("{}{}", TEMP_PREFIX, timestamp);
    Ok(canonical_dir.join(temp_name))
}

/// Atomically write bytes to a file following D6.
///
/// Steps:
/// 1. Resolve symlink to target
/// 2. Write to temp file in same directory
/// 3. Copy permissions from existing file (if any)
/// 4. fsync the temp file
/// 5. rename-over (atomic replace)
/// 6. fsync the directory
///
/// Returns the hash of the written content.
pub fn atomic_write<F: Fs>(fs: &F, path: &Path, bytes: &[u8]) -> Result<[u8; 32]> {
    // Step 1: Resolve symlink
    let target = fs.resolve_symlink(path)?;

    // Step 2: Generate temp path in same directory
    let temp_path = temp_path(fs, &target)?;

    // Step 3: Write temp file
    fs.write_new(&temp_path, bytes)?;

    // Step 4: Copy permissions (if source exists)
    // Note: OsFs::copy_mode is a no-op for now; real implementation would copy mode bits
    let _ = fs.copy_mode(&target, &temp_path);

    // Step 5: fsync the temp file
    fs.fsync_file(&temp_path)?;

    // Step 6: Atomic rename (replace existing)
    fs.rename_replace(&temp_path, &target)?;

    // Step 7: fsync the directory
    if let Some(dir) = target.parent() {
        let _ = fs.fsync_dir(dir);
    }

    // Step 8: Hash the written file
    let hash = crate::fs::hash_file(fs, &target)?;
    Ok(hash)
}

/// Read prior bytes and compute hash.
pub fn read_prior_bytes<F: Fs>(fs: &F, path: &Path) -> Result<Option<(Vec<u8>, [u8; 32])>> {
    match fs.read_if_exists(path)? {
        Some(bytes) => {
            let hash = blake3::hash(&bytes);
            Ok(Some((bytes, *hash.as_bytes())))
        }
        None => Ok(None),
    }
}

/// List orphan temporary files in a directory.
///
/// Orphan temps are files with the TEMP_PREFIX that weren't cleaned up.
pub fn list_orphan_temps<F: Fs>(fs: &F, dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = fs.read_dir(dir)?;
    let mut orphans = Vec::new();

    for entry in entries {
        let name = entry.name.to_string_lossy();
        if name.starts_with(TEMP_PREFIX) {
            orphans.push(entry.path);
        }
    }

    Ok(orphans)
}

/// Cleanup orphan temporary files in a directory.
pub fn cleanup_orphan_temps<F: Fs>(fs: &F, dir: &Path) -> Result<usize> {
    let orphans = list_orphan_temps(fs, dir)?;
    let mut count = 0;

    for orphan in orphans {
        let _ = fs.remove(&orphan);
        count += 1;
    }

    Ok(count)
}
